# ADR-0008: Preflight chunk headers and preserve node admission rejections

- **Status:** Proposed
- **Date:** 2026-11-10
- **Decision owners:** mickvandijke
- **Reviewers:** Client and node maintainers, pending review
- **Supersedes:** none
- **Superseded by:** none
- **Related:** [V2-1911](https://linear.app/autonominetwork/issue/V2-1911), ADR-0002, ant-node ADR-0030

## Context

Nodes are adding a bounded recognizable-file-header gate to client chunk PUTs.
They retain the existing StorageFailed wire variant. The client currently
classifies every such response as disk-full and can retry unchanged bytes,
even though a recognizable header requires different content. Raw chunk APIs
also pay before learning whether the nodes will accept the bytes.

## Decision Drivers

- Refuse predictable failures before quotes and irreversible on-chain payment.
- Preserve normal self-encrypted file uploads and raw opaque chunk semantics.
- Treat content admission as an application error, without throttling the network limiter.
- Keep one peer from vetoing a healthy upload, and preserve paid/stored progress.

## Considered Options

1. Leave the rejection as a retryable generic storage failure.
2. Automatically self-encrypt raw chunk API input, changing its address and return contract.
3. Preflight the node policy and propagate a dedicated terminal application error.

## Decision

Use option 3. Mirror the node's vetted `file-format` 0.29 policy with deep
readers disabled. Validate before raw chunk payment, chunk preparation and
batch wallet submission, and in the shared native/browser upload preflight.
Validate proof-backed sends as well. Ordinary file uploads already self-encrypt
their data; raw chunk APIs continue accepting opaque bytes without transforming them.
The CLI reads file/stdin bytes and calls the public `data::validate_chunk_upload`
before wallet or network setup, then passes those same bytes into execution.
Missing credentials or an invalid bootstrap manifest must not hide this error.

Recognize exactly this node StorageFailed message:

> chunk has a recognizable file header; self-encrypt files before uploading

Expose `data::Error::ChunkUploadRejected { address }`. The message is a
compatibility contract with the node PR, without a new wire discriminant.
Classify it as an application rejection rather than disk-full or congestion.
Single-peer callers receive the typed rejection directly. Close-group uploads
still try fallback peers and accept a successful quorum. Only a failed store
with matching refusals from CLOSE_GROUP_MAJORITY distinct peers turns the
remote claim into a terminal chunk verdict.

Do not retry a terminally rejected chunk. Carry the typed cause through wave
results and partial-upload errors so subsequent waves are not paid, while
settled spend and completed stores remain reportable. Merkle storage treats the
new error as fatal rather than a recoverable quorum shortfall. CLI help points
ordinary file users to `ant file upload`.

## Consequences

### Positive

- Predictable recognizable-header failures happen before payment where bytes are available.
- Native and browser uploads use the same content and error policy.
- A single rejecting peer does not prevent a successful close-group upload.

### Negative / Trade-offs

- This remains a heuristic; disguised or unsupported headers can pass.
- Node/client policy copies and classifier versions must stay aligned until shared protocol tooling owns them.
- The new public Error variant and WaveResult terminal_error field affect exhaustive matches and struct literals.
- Address-only quote APIs cannot preflight content they have not received.
- Already-submitted payments cannot be refunded by returning this error.

### Neutral / Operational

- No new wire format or stored-data layout; older nodes continue accepting encrypted chunks.
- Roll back client preflight/error handling together with the node admission gate if needed.
- A mixed-version dev-testnet test is required before release, beyond local unit checks.

## Validation

Keep header fixtures aligned with the node, accept real self-encryption output,
and exercise exact remote-message decoding, disk-full separation, application
limiter classification, corroboration thresholds, fallback success, pre-payment
refusal, and terminal retry behavior. Run native and browser/WASM build checks,
targeted upload tests, formatting, strict Clippy, and ADR governance checks.

`ant-core/tests/e2e_chunk_admission.rs` is an opt-in paired-branch check until
an enforcing node release is available. Run it with a local Cargo
`patch.crates-io.ant-node.path` pointing to the companion node checkout and
`--test e2e_chunk_admission -- --ignored --nocapture`; do not commit the
temporary lockfile change. It starts 14 real QUIC nodes and Anvil, probes the
node directly to bypass client preflight, checks that rejected inputs are
absent from storage and cost no tokens or gas, then uploads and downloads
self-encrypted file bytes with a real payment.
The Chromium admission test in `browser-tests/webrtc.spec.js` also checks
actionable errors before payment and a paid encrypted-file round trip over
real WebRTC. Set `ANT_NODE_DIR` to the companion checkout for paired validation;
ordinary browser CI keeps its released-node compatibility pin.

## Notes for AI-assisted work

This ADR remains Proposed until human review. Accepted ADRs are immutable.
