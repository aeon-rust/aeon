//! S10 outbound-auth → rdkafka `ClientConfig` translation.
//!
//! Shared by `KafkaSource` and `KafkaSink`. The signer is already resolved
//! (`SecretBytes` / typed enum) by the time we get here — this module only
//! maps modes onto the rdkafka knob namespace (`security.protocol`,
//! `sasl.*`, `ssl.*`).
//!
//! **One mode per connector** — if a misfit mode (Bearer / Basic / ApiKey /
//! HmacSign) reaches the Kafka transport, it's warned-and-ignored rather
//! than translated, matching S9's inbound-auth policy: operators see the
//! mismatch in logs but the pipeline still comes up.

use aeon_types::{OutboundAuthMode, OutboundAuthSigner};
use rdkafka::config::ClientConfig;
use std::sync::Arc;

/// Apply an outbound auth signer onto the given `ClientConfig`. Silently
/// does nothing if `signer` is `None`. Intended to be called AFTER the
/// base rdkafka knobs are set but BEFORE user `config_overrides` — so the
/// user can still hard-override any knob for debugging.
pub(super) fn apply_outbound_auth(
    client_config: &mut ClientConfig,
    signer: Option<&Arc<OutboundAuthSigner>>,
) {
    let Some(s) = signer else { return };
    match s.mode() {
        OutboundAuthMode::None => {
            // Explicit no-op — relies on network-layer auth (IP allow-list,
            // VPC peering, SG). Nothing to set.
        }
        OutboundAuthMode::BrokerNative => {
            // Pass-through: operator supplies rdkafka knob names directly
            // (`security.protocol`, `sasl.mechanism`, `sasl.username`,
            // `sasl.password`, `sasl.oauthbearer.config`, `ssl.ca.location`,
            // etc.). Aeon is not a schema enforcer for the broker SDK.
            if let Some(bn) = s.broker_native() {
                for (k, v) in &bn.values {
                    client_config.set(k.as_str(), v.as_str());
                }
            }
        }
        OutboundAuthMode::Mtls => {
            // librdkafka accepts inline PEM via `ssl.certificate.pem` and
            // `ssl.key.pem` (librdkafka ≥ 1.5). Raising `security.protocol`
            // to `ssl` covers the classic mTLS case; the operator can add
            // `sasl_ssl` via `config_overrides` when combining SASL + TLS.
            let cert = s.mtls_cert_pem().unwrap_or_default();
            let key = s.mtls_key_pem().unwrap_or_default();
            client_config.set("security.protocol", "ssl");
            if !cert.is_empty() {
                // Invalid UTF-8 in a PEM would be operator error — rdkafka
                // will fail on connect with a clear message, no need to
                // second-guess here.
                if let Ok(cert_str) = std::str::from_utf8(cert) {
                    client_config.set("ssl.certificate.pem", cert_str);
                }
            }
            if !key.is_empty() {
                if let Ok(key_str) = std::str::from_utf8(key) {
                    client_config.set("ssl.key.pem", key_str);
                }
            }
        }
        OutboundAuthMode::Bearer
        | OutboundAuthMode::Basic
        | OutboundAuthMode::ApiKey
        | OutboundAuthMode::HmacSign => {
            tracing::warn!(
                mode = ?s.mode(),
                "Kafka connector: HTTP-style auth mode is not applicable to the Kafka protocol; \
                 ignored. Use broker_native or mtls.",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{BrokerNativeConfig, OutboundAuthConfig, OutboundMtlsConfig};
    use std::collections::BTreeMap;

    fn signer(config: OutboundAuthConfig) -> Arc<OutboundAuthSigner> {
        Arc::new(OutboundAuthSigner::build(config).unwrap())
    }

    #[test]
    fn none_signer_leaves_config_unchanged() {
        let mut cc = ClientConfig::new();
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::None,
            ..Default::default()
        });
        apply_outbound_auth(&mut cc, Some(&s));
        assert!(cc.get("security.protocol").is_none());
        assert!(cc.get("sasl.mechanism").is_none());
    }

    #[test]
    fn broker_native_passes_through_values() {
        let mut cc = ClientConfig::new();
        let mut values = BTreeMap::new();
        values.insert("security.protocol".to_string(), "sasl_ssl".to_string());
        values.insert("sasl.mechanism".to_string(), "SCRAM-SHA-512".to_string());
        values.insert("sasl.username".to_string(), "aeon".to_string());
        values.insert("sasl.password".to_string(), "s3cret".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        apply_outbound_auth(&mut cc, Some(&s));
        assert_eq!(cc.get("security.protocol").unwrap(), "sasl_ssl");
        assert_eq!(cc.get("sasl.mechanism").unwrap(), "SCRAM-SHA-512");
        assert_eq!(cc.get("sasl.username").unwrap(), "aeon");
        assert_eq!(cc.get("sasl.password").unwrap(), "s3cret");
    }

    #[test]
    fn mtls_sets_ssl_knobs() {
        let mut cc = ClientConfig::new();
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Mtls,
            mtls: Some(OutboundMtlsConfig {
                cert_pem: "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n"
                    .to_string(),
                key_pem: "-----BEGIN PRIVATE KEY-----\nMIIE\n-----END PRIVATE KEY-----\n"
                    .to_string(),
            }),
            ..Default::default()
        });
        apply_outbound_auth(&mut cc, Some(&s));
        assert_eq!(cc.get("security.protocol").unwrap(), "ssl");
        assert!(
            cc.get("ssl.certificate.pem")
                .unwrap()
                .contains("BEGIN CERTIFICATE")
        );
        assert!(
            cc.get("ssl.key.pem")
                .unwrap()
                .contains("BEGIN PRIVATE KEY")
        );
    }

    #[test]
    fn bearer_mode_warns_and_skips() {
        let mut cc = ClientConfig::new();
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Bearer,
            bearer: Some(aeon_types::BearerConfig {
                token: "x".to_string(),
            }),
            ..Default::default()
        });
        apply_outbound_auth(&mut cc, Some(&s));
        // No Kafka-side knob should have been touched.
        assert!(cc.get("security.protocol").is_none());
        assert!(cc.get("sasl.mechanism").is_none());
    }

    #[test]
    fn none_applied_with_no_signer_is_noop() {
        let mut cc = ClientConfig::new();
        apply_outbound_auth(&mut cc, None);
        assert!(cc.get("security.protocol").is_none());
    }
}
