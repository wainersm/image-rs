// Copyright (c) 2022 Alibaba Cloud
//
// SPDX-License-Identifier: Apache-2.0
//

use std::{io::Write, str::FromStr};

use anyhow::{Context, Result};
use async_trait::async_trait;
use attester::{detect_tee_type, BoxedAttester};
use tokio::sync::Mutex;

pub use attester::InitdataResult;

pub mod config;
mod eventlog;
pub mod token;

use config::HashAlgorithm;
use eventlog::{EventEntry, EventLog};
use log::{info, warn};
use token::*;

use crate::config::Config;

/// Attestation Agent (AA for short) is a rust library crate for attestation procedure
/// in confidential containers. It provides kinds of service APIs related to attestation,
/// including the following
/// - `get_token`: get attestation token from remote services, e.g. attestation services.
/// - `get_evidence`: get hardware TEE signed evidence due to given runtime_data, s.t.
/// report data.
/// - `extend_runtime_measurement`: extend the runtime measurement. This will extend the
/// current hardware runtime measurement register (if any) or PCR for (v)TPM (under
/// development) platforms
/// with a runtime event.
/// - `check_init_data`: check if the given data slice matches the current confidential
/// computing environment's host data field, e.g. MRCONFIGID for TDX, HOSTDATA for SNP.
///
/// # Example
///
/// ```no_run
/// use attestation_agent::AttestationAgent;
/// use attestation_agent::AttestationAPIs;
///
/// // initialize with empty config
/// let mut aa = AttestationAgent::new(None).unwrap();
///
/// let _quote = aa.get_evidence(&[0;64]);
/// ```

/// `AttestationAPIs` defines the service APIs of attestation agent that need to make requests
///  to the Relying Party (Key Broker Service) in Confidential Containers.
///
/// For every service API, the `kbc_name` and `kbs_uri` is necessary, `kbc_name` tells
/// attestation agent which KBC module it should use and `kbs_uri` specifies the KBS address.
#[async_trait]
pub trait AttestationAPIs {
    /// Get attestation Token
    async fn get_token(&mut self, token_type: &str) -> Result<Vec<u8>>;

    /// Get TEE hardware signed evidence that includes the runtime data.
    async fn get_evidence(&mut self, runtime_data: &[u8]) -> Result<Vec<u8>>;

    /// Extend runtime measurement register
    async fn extend_runtime_measurement(
        &mut self,
        domain: &str,
        operation: &str,
        content: &str,
        register_index: Option<u64>,
    ) -> Result<()>;

    /// Check the initdata binding
    async fn check_init_data(&mut self, init_data: &[u8]) -> Result<InitdataResult>;
}

/// Attestation agent to provide attestation service.
pub struct AttestationAgent {
    config: Config,
    attester: BoxedAttester,
    eventlog: Mutex<EventLog>,
}

impl AttestationAgent {
    pub async fn init(&mut self) -> Result<()> {
        // We should get the current platform's evidence to see the RTMR value.
        // Here we assume RTMR is not polluted thus all be set `\0`
        let init_entry = match self.config.eventlog_config.eventlog_algorithm {
            HashAlgorithm::Sha256 => "INIT sha256/0000000000000000000000000000000000000000000000000000000000000000",
            HashAlgorithm::Sha384 => "INIT sha384/000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
            HashAlgorithm::Sha512 => "INIT sha512/00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        };

        let event_digest = self
            .config
            .eventlog_config
            .eventlog_algorithm
            .digest(init_entry.as_bytes());

        let mut eventlog = self.eventlog.lock().await;

        self.attester
            .extend_runtime_measurement(event_digest, self.config.eventlog_config.init_pcr)
            .await
            .context("write INIT entry")?;
        eventlog.write_log(init_entry).context("write INIT log")?;

        Ok(())
    }

    /// Create a new instance of [AttestationAgent].
    pub fn new(config_path: Option<&str>) -> Result<Self> {
        let config = match config_path {
            Some(config_path) => {
                info!("Using AA config file: {config_path}");
                Config::try_from(config_path)?
            }
            None => {
                warn!("No AA config file specified. Using a default configuration.");
                Config::new()?
            }
        };

        let tee_type = detect_tee_type();
        let attester: BoxedAttester = tee_type.try_into()?;
        let eventlog = Mutex::new(EventLog::new()?);

        Ok(AttestationAgent {
            config,
            attester,
            eventlog,
        })
    }

    /// This is a workaround API for initdata in CoCo. Once
    /// a better design is implemented we can deprecate the API.
    /// See https://github.com/kata-containers/kata-containers/issues/9468
    pub fn update_configuration(&mut self, conf: &str) -> Result<()> {
        let mut tmpfile = tempfile::NamedTempFile::new()?;
        let _ = tmpfile.write(conf.as_bytes())?;
        tmpfile.flush()?;

        let config = Config::try_from(
            tmpfile
                .path()
                .as_os_str()
                .to_str()
                .expect("tempfile will not create non-unicode char"),
            // Here we can use `expect()` because tempfile crate will generate file name
            // only including numbers and alphabet (0-9, a-z, A-Z)
        )?;
        self.config = config;
        Ok(())
    }
}

/// Default PCR index used by AA. `17` is selected for its usage of dynamic root of trust for measurement.
/// - [Linux TPM PCR Registry](https://uapi-group.org/specifications/specs/linux_tpm_pcr_registry/)
/// - [TCG TRUSTED BOOT CHAIN IN EDK II](https://tianocore-docs.github.io/edk2-TrustedBootChain/release-1.00/3_TCG_Trusted_Boot_Chain_in_EDKII.html)
const DEFAULT_PCR_INDEX: u64 = 17;

#[async_trait]
impl AttestationAPIs for AttestationAgent {
    async fn get_token(&mut self, token_type: &str) -> Result<Vec<u8>> {
        let token_type = TokenType::from_str(token_type).context("Unsupported token type")?;

        match token_type {
            #[cfg(feature = "kbs")]
            token::TokenType::Kbs => {
                token::kbs::KbsTokenGetter::new(&self.config.token_configs.kbs)
                    .get_token()
                    .await
            }
            #[cfg(feature = "coco_as")]
            token::TokenType::CoCoAS => {
                token::coco_as::CoCoASTokenGetter::new(&self.config.token_configs.coco_as)
                    .get_token()
                    .await
            }
        }
    }

    /// Get TEE hardware signed evidence that includes the runtime data.
    async fn get_evidence(&mut self, runtime_data: &[u8]) -> Result<Vec<u8>> {
        let evidence = self.attester.get_evidence(runtime_data.to_vec()).await?;
        Ok(evidence.into_bytes())
    }

    /// Extend runtime measurement register. Parameters
    /// - `events`: a event slice. Any single event will be calculated into a hash digest to extend the current
    /// platform's RTMR.
    /// - `register_index`: a target PCR that will be used to extend RTMR. Note that different platform
    /// would have its own strategy to map a PCR index into a architectual RTMR index. If not given, a default one
    /// will be used.
    async fn extend_runtime_measurement(
        &mut self,
        domain: &str,
        operation: &str,
        content: &str,
        register_index: Option<u64>,
    ) -> Result<()> {
        let register_index = register_index.unwrap_or_else(|| {
            info!("No PCR index provided, use default {DEFAULT_PCR_INDEX}");
            DEFAULT_PCR_INDEX
        });

        let log_entry = EventEntry::new(domain, operation, content);
        let event_digest = log_entry.digest_with(self.config.eventlog_config.eventlog_algorithm);

        let mut eventlog = self.eventlog.lock().await;

        self.attester
            .extend_runtime_measurement(event_digest, register_index)
            .await?;

        eventlog.write_log(&log_entry.to_string())?;

        Ok(())
    }

    /// Check the initdata binding. If current platform does not support initdata
    /// injection, return `InitdataResult::Unsupported`.
    async fn check_init_data(&mut self, init_data: &[u8]) -> Result<InitdataResult> {
        self.attester.check_init_data(init_data).await
    }
}
