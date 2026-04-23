# Aeon Compliance Guide

This document is the operator-facing guide for running Aeon under a regulatory
regime. It covers the three regimes the engine enforces natively — **PCI-DSS**,
**HIPAA**, and **GDPR** — plus the `Mixed` mode for pipelines that touch data
falling under more than one regime.

For the underlying security controls (at-rest encryption, secret management,
audit log channel, inbound/outbound connector auth, SSRF hardening), see
[`SECURITY.md`](SECURITY.md) §11. This document focuses on how to *declare*
a regime and *what the engine will enforce* at pipeline start.

---

## Table of Contents

1. [How Compliance Enforcement Works](#1-how-compliance-enforcement-works)
2. [Pipeline YAML Surface](#2-pipeline-yaml-surface)
3. [PCI-DSS](#3-pci-dss)
4. [HIPAA](#4-hipaa)
5. [GDPR](#5-gdpr)
6. [Mixed Regime](#6-mixed-regime)
7. [PII / PHI Selectors](#7-pii--phi-selectors)
8. [GDPR Right-to-Erasure and Right-to-Export](#8-gdpr-right-to-erasure-and-right-to-export)
9. [Operator Checklist per Regime](#9-operator-checklist-per-regime)
10. [Known Gaps and Follow-Ups](#10-known-gaps-and-follow-ups)

---

## 1. How Compliance Enforcement Works

Every pipeline has a `compliance` block in its YAML manifest. At pipeline
start, the engine's `compliance_validator::validate_compliance` runs before
the first event is accepted. It consumes:

- The declared `ComplianceBlock` (regime, enforcement level, selectors,
  erasure policy).
- The resolved `EncryptionPlan` (from the `state.l2` / `state.l3` encryption
  config).
- The resolved `RetentionPlan` (from the retention config).
- The resolved erasure surface (from the S6 subject-id + tombstone store
  config).

It then checks, per regime, whether all required preconditions are met.
Outcomes depend on the enforcement level:

| Enforcement | Behavior on missing precondition |
|---|---|
| `off` (default) | No checks. Appropriate for dev only. |
| `warn` | Findings logged as warnings; pipeline starts. |
| `strict` | Findings are hard gates; pipeline refuses to start. |

**Production pipelines should use `strict`.** `warn` is for migration windows
— the phase where you're adding compliance to an existing pipeline and want
to see the finding list without yet taking an outage.

Regime-to-precondition map (from `ComplianceRegime::requires_*` in
`crates/aeon-types/src/compliance.rs`):

| Regime | Encryption | Retention | Erasure |
|---|---|---|---|
| `none` | — | — | — |
| `pci` | required | required | not required |
| `hipaa` | required | required | not required |
| `gdpr` | required | required | required |
| `mixed` | required | required | required |

---

## 2. Pipeline YAML Surface

The `compliance` block slots into the pipeline manifest alongside
`durability` (see [`EO-2-DURABILITY-DESIGN.md`](EO-2-DURABILITY-DESIGN.md)):

```yaml
pipelines:
  - name: payments-enrich
    source: { ... }
    sink: { ... }
    processor: { ... }

    # S4 — compliance block.
    compliance:
      regime: pci                 # none | pci | hipaa | gdpr | mixed
      enforcement: strict         # off | warn | strict
      selectors:                  # S4.1 — PII / PHI field selectors
        - path: "$.card.pan"
          format: json
          class: pii
        - path: "$.card.cvv"
          format: json
          class: pii
      erasure:                    # S6.8 — only honored when regime ∈ {gdpr, mixed}
        max_delay_hours: 24       # 24h default; Art. 17 SLA is 30 days

    # S3 — required when compliance.regime requires encryption.
    state:
      l2:
        encryption: { mode: aes_256_gcm, kek_ref: vault://kek/payments }
      l3:
        encryption: { mode: aes_256_gcm, kek_ref: vault://kek/payments }

    # S5 — required when compliance.regime requires retention.
    retention:
      l2_body: 7d
      l3_ack: 24h
```

Defaults are **inert**: a pipeline without a `compliance` block (or with
`regime: none` + `enforcement: off`) runs with no compliance-layer overhead.
That is the right default for dev and non-regulated workloads.

---

## 3. PCI-DSS

Use `regime: pci` for pipelines handling cardholder data (PAN, CVV, expiry,
cardholder name).

### What the validator requires

- **At-rest encryption** on both L2 body and L3 checkpoints
  (`state.l2.encryption` + `state.l3.encryption`), AES-256-GCM.
- **KEK sourced from a production secret provider** (Vault, AWS KMS, AWS SM,
  GCP SM). Literal YAML values or bare env-var KEKs fail strict mode.
- **Retention** configured on both tiers. PCI-DSS 10.7 requires 1 year of
  audit log retention with 3 months online; translate to your audit sink's
  policy, not Aeon's L2/L3 retention (those are shorter by design).
- **Tracing redaction** on (§11.6 in [`SECURITY.md`](SECURITY.md)). PAN must
  never reach a log line.
- **Audit log channel** routed to a separate sink (§11.7). PCI-DSS 10.5
  requires audit trails be protected from unauthorized modification.
- **KEK rotation** configured (primary + next). PCI-DSS 3.6.4 requires
  periodic key rotation.

### YAML example

```yaml
compliance:
  regime: pci
  enforcement: strict
  selectors:
    - { path: "$.card.pan",    format: json, class: pii }
    - { path: "$.card.cvv",    format: json, class: pii }
    - { path: "$.card.expiry", format: json, class: pii }
```

### Notes

- The validator does not prove PCI-DSS compliance end-to-end — it gates the
  *technical preconditions* that make compliance possible. Network
  segmentation, operator access review, vulnerability scanning, and penetration
  testing remain the operator's responsibility.
- The SSRF policy (§11.8 in [`SECURITY.md`](SECURITY.md)) should be left at
  the production profile for PCI-DSS pipelines — cardholder data should not
  egress to arbitrary URLs.

---

## 4. HIPAA

Use `regime: hipaa` for pipelines handling Protected Health Information (PHI).

### What the validator requires

- **At-rest encryption** on both L2 body and L3 checkpoints (§11.3 in
  [`SECURITY.md`](SECURITY.md)). HIPAA Security Rule §164.312(a)(2)(iv)
  treats encryption as an addressable safeguard; the validator makes it
  required so the question is settled.
- **KEK sourced from a production secret provider.**
- **Retention** configured. HIPAA does not mandate a specific retention
  floor in the Security Rule, but record-retention obligations from other
  statutes (state medical-records laws, FDA 21 CFR, etc.) almost always
  do. Configure your L2/L3 retention to match the *shortest* applicable
  floor.
- **Tracing redaction** on. PHI must never reach a log line.
- **Audit log channel** routed to a separate sink. HIPAA §164.312(b)
  requires audit controls.
- **KEK rotation** configured.

### YAML example

```yaml
compliance:
  regime: hipaa
  enforcement: strict
  selectors:
    - { path: "$.patient.mrn",         format: json, class: phi }
    - { path: "$.patient.ssn",         format: json, class: phi }
    - { path: "$.patient.dob",         format: json, class: phi }
    - { path: "$.encounter.diagnosis", format: json, class: phi }
```

### Notes

- HIPAA's Minimum Necessary rule (§164.502(b)) is an *operational* control,
  not a technical one. The engine does not enforce minimum-necessary at the
  pipeline level — that is a pipeline-authoring concern (don't ingest PHI
  you don't need).
- For BAAs (Business Associate Agreements) with downstream services (a data
  warehouse, an ML training system), use outbound auth (§11.11 in
  [`SECURITY.md`](SECURITY.md)) with `mtls` or `hmac_sign` rather than
  `bearer` — the stronger modes make the trust boundary explicit.

---

## 5. GDPR

Use `regime: gdpr` for pipelines processing personal data of EU data
subjects (Articles 4(1), 4(2)).

### What the validator requires

- **At-rest encryption** on both L2 body and L3 checkpoints. Article 32 lists
  pseudonymization and encryption as appropriate technical measures.
- **KEK sourced from a production secret provider.**
- **Retention** configured. Article 5(1)(e) (storage limitation) requires
  personal data to be retained no longer than necessary; the validator
  enforces that *some* retention ceiling is declared, not that it matches
  any specific value.
- **Tracing redaction** on.
- **Audit log channel** routed to a separate sink.
- **Erasure surface** configured:
  - A subject-id extractor (`SubjectIdExtractor` — per-pipeline)
  - A tombstone store wired up
  - A deny-list enforced at ingest
  - Right-to-export endpoint enabled
- **`erasure.max_delay_hours`** set. Default 24h; Article 17 gives 30 days
  from the data-subject's request. The validator ensures
  `max_delay_hours × 30 + observed_compaction_lag ≤ 30 days` in spirit,
  but in practice the default leaves 29 days of slack.

### YAML example

```yaml
compliance:
  regime: gdpr
  enforcement: strict
  selectors:
    - { path: "$.user.email",   format: json, class: pii }
    - { path: "$.user.ip",      format: json, class: pii }
    - { path: "$.user.device",  format: json, class: pii }
  erasure:
    max_delay_hours: 24

subject_id:
  path: "$.user.id"
  format: json
```

### Notes

- The `subject_id` block (outside `compliance`) declares how to extract the
  data-subject identifier from each event. This is what the erasure API
  takes as input.
- For pipelines processing children's data (Article 8) or special categories
  (Article 9), additional operator-side controls apply — the engine does not
  distinguish these categories in schema.

---

## 6. Mixed Regime

Use `regime: mixed` when a single pipeline carries records falling under more
than one regime — e.g. a payment receipt that also includes patient data, or
a GDPR-scoped user profile that also has a card-on-file.

### What the validator requires

The **union** of all applicable regimes' preconditions. In practice, this
means:

- All PCI requirements.
- All HIPAA requirements (if PHI is in scope).
- All GDPR requirements (including erasure + export, if EU subjects are in
  scope).

Declare every applicable regime's selectors in the `selectors` list with the
right `class` (`pii` / `phi`).

### YAML example

```yaml
compliance:
  regime: mixed
  enforcement: strict
  selectors:
    - { path: "$.card.pan",    format: json, class: pii }
    - { path: "$.patient.mrn", format: json, class: phi }
    - { path: "$.user.email",  format: json, class: pii }
  erasure:
    max_delay_hours: 24
```

### Notes

- `mixed` is deliberately the strictest available mode. If you're not sure
  which regime applies, `mixed` is the safe choice — the validator will
  gate on the maximum-set of preconditions.

---

## 7. PII / PHI Selectors

Selectors (`ComplianceBlock.selectors`) describe *where* the sensitive fields
live in the event payload. They are consumed by:

- **S3 at-rest encryption** — to ensure the fields are included in the
  encrypted region (L2 body, L3 checkpoints).
- **S5 retention** — to apply field-level redaction on retention expiry
  (future work; currently whole-event retention).
- **S6 erasure** — to scrub field contents when a subject is erased.

### Supported payload formats

| Format | `path` syntax | Notes |
|---|---|---|
| `json` | JSONPath-style: `$.user.ssn` | Default. |
| `message_pack` | JSONPath-style: `$.user.ssn` | Decoded on scan. |
| `binary_length_prefix` | Byte-offset: `0:16` for bytes 0–15 | Format-specific; scanner resolves offsets. |

Protobuf is intentionally not supported — schema-registry integration is a
separate future initiative. If your pipeline ingests protobuf, transcode to
JSON or MessagePack at the edge, or accept that per-field controls will not
apply.

### Example

```yaml
selectors:
  - { path: "$.card.pan",                 format: json,                  class: pii }
  - { path: "$.patient.mrn",              format: json,                  class: phi }
  - { path: "0:16",                       format: binary_length_prefix,  class: pii }
  - { path: "$.meta.geo.country",         format: message_pack,          class: pii }
```

---

## 8. GDPR Right-to-Erasure and Right-to-Export

GDPR Articles 17 (erasure) and 20 (portability) are the two operator
workflows Aeon supports natively.

### Right-to-Erasure

1. Operator receives a request from a data subject.
2. Look up the subject's canonical identifier (whatever the pipeline's
   `subject_id` extractor returns — email, user-id, etc.).
3. Call the erasure API:

   ```http
   POST /api/v1/pipelines/{name}/erase
   Authorization: Bearer $AEON_API_TOKEN
   Content-Type: application/json

   { "subject_id": "user-12345" }
   ```

4. The engine writes a tombstone to the deny-list store.
5. Future reads of events for that subject are suppressed.
6. Future writes for that subject are rejected at ingest.
7. Within `erasure.max_delay_hours` (default 24h), a compaction sweep
   physically removes affected events from L2 body + L3 checkpoints.

### Right-to-Export

1. Operator receives a portability request.
2. Call the export API:

   ```http
   GET /api/v1/pipelines/{name}/export?subject_id=user-12345
   Authorization: Bearer $AEON_API_TOKEN
   ```

3. The engine returns all events for that subject across the retention
   window, as a newline-delimited JSON stream.
4. If the subject has already been erased, the response is a **cryptographic
   null-receipt**: a signed attestation that
   1. the subject's events are absent, and
   2. the PoH/Merkle chain still verifies without them.

   This is what distinguishes Aeon's erasure from a simple DELETE — downstream
   auditors can verify that the chain is intact *despite* the erasure.

### Operational SLA

- Art. 17 SLA: 30 days from subject request.
- Engine default: tombstone → deny-list takes effect immediately;
  physical compaction within 24h (`erasure.max_delay_hours`).
- Headroom: 29 days between physical compaction and the statutory deadline.

---

## 9. Operator Checklist per Regime

### PCI-DSS

- [ ] `compliance.regime: pci`, `enforcement: strict`.
- [ ] All card fields listed in `selectors` with `class: pii`.
- [ ] L2 + L3 encryption on, KEK sourced from a `SecretProvider` (Env / DotEnv today; Vault / OpenBao / KMS / SM once the `aeon-secrets` adapter lands — task #35).
- [ ] Retention configured on both tiers.
- [ ] Audit channel routed to immutable sink.
- [ ] KEK rotation schedule (primary + next) established.
- [ ] Outbound sinks use `mtls` or `broker_native` — not plain `bearer`.
- [ ] SSRF policy at production profile.
- [ ] Pipeline tested with `aeon apply --dry-run`.

### HIPAA

- [ ] `compliance.regime: hipaa`, `enforcement: strict`.
- [ ] All PHI fields listed in `selectors` with `class: phi`.
- [ ] L2 + L3 encryption on, KEK sourced from a `SecretProvider` (Env / DotEnv today; Vault / OpenBao / KMS / SM once the `aeon-secrets` adapter lands — task #35).
- [ ] Retention matches applicable statutory floor.
- [ ] Audit channel separated and protected.
- [ ] BAA-covered downstream services authenticated with `mtls` or
      `hmac_sign`.
- [ ] Pipeline tested with `aeon apply --dry-run`.

### GDPR

- [ ] `compliance.regime: gdpr`, `enforcement: strict`.
- [ ] Subject-id extractor configured.
- [ ] All personal-data fields in `selectors`.
- [ ] Erasure API tested against a sample subject before go-live.
- [ ] Right-to-export tested; null-receipt verified for an erased subject.
- [ ] `erasure.max_delay_hours` set (24h default is usually fine).
- [ ] L2 + L3 encryption on, KEK sourced from a `SecretProvider` (Env / DotEnv today; Vault / OpenBao / KMS / SM once the `aeon-secrets` adapter lands — task #35).
- [ ] Retention ceiling configured (not just floor).
- [ ] DPO has visibility into the audit channel.

### Mixed

- [ ] Every applicable regime's items above.

---

## 10. Known Gaps and Follow-Ups

- **Field-level retention / redaction on retention expiry** — today,
  retention is whole-event. Field-level scrubbing (keep the event but strip
  PII/PHI fields once their retention window passes) is a design point on
  the post-Gate-2 roadmap, not in current code.
- **Protobuf payload support** — intentionally out of scope; transcode at
  the edge.
- **FIPS 140-3 compliance claim** — available today via cloud KMS (AWS
  KMS, GCP Cloud KMS, Azure Key Vault are themselves FIPS 140-3 L3) and
  via Vault / OpenBao with an HSM seal. Direct PKCS#11 integration inside
  Aeon (task #33) is deferred; the S1.4 trait stub is the extension point
  for the air-gapped case where Vault / OpenBao is not an option. See
  [`SECURITY.md`](SECURITY.md) §11.2 for the FIPS path table.
- **Audit channel call-site wiring** — the channel itself is in place
  (S2.5). Call sites for KEK rotation, config changes, authentication
  failures are ongoing; some paths still log to the data-path tracing
  channel and will be migrated in follow-ups.
- **SOC 2 / ISO 27001** — these are organizational certifications, not
  engine-level regimes. Aeon's technical controls contribute to the
  evidence trail for those certifications but the engine does not declare
  or enforce them as a `regime` value.

---

*See also:* [`SECURITY.md`](SECURITY.md) · [`EO-2-DURABILITY-DESIGN.md`](EO-2-DURABILITY-DESIGN.md) · [`ROADMAP.md`](ROADMAP.md) · [`POSITIONING.md`](POSITIONING.md)
