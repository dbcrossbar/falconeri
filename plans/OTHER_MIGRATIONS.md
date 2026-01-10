# Post-Async Migration

> **Status:** _Mostly_ done. Look at unchecked items below, see if they've been addressed, and consider adding them to Beads once we set it up.

- [x] Latest diesel-async?
- [x] What if babysitter exits silently or a the future is dropped? Are those real cases we need to worry about? If not, document. If so, add appropriate hard failures in unexpected places.
- [x] `db::async_pool(1, ConnectVia::Proxy)` -> db::async_client_pool() wrapper or something like that.
- [x] Fix Command to use async Tokio equivalent
   - [x] `crossbeam::scope` in `falconeri-worker/src/main.rs` probably should be replaced with async Command invocation and appropriate subprocess I/O code, which may allow us to drop crossbeam as a dependency.

Later

- [x] structopt -> clap derive
- [x] Fix main/run split to report errors by returning anything::Result from main(), which should do the same thing in a more standard way.
- [x] Deal with advisories in `deny.toml`
- [x] Add `#[instrument]` to more of our async functions for better tracing.
- [x] Get manual integration test working with minikube.
- [x] Bump version number
- [x] Move shared dependencies back into falconeri_common/Cargo.toml whenever possible, or use the new "workspace" feature of Cargo.toml to manage them centrally.
- [x] Update AGENTS.md to explain how to test 
- [x] Auto-fmt imports
- [ ] Make sure we're still validating credentials! And add a test.
- [ ] Make sure killing the proxy kills all kubectl port-forwards too.

Image update

- [x] Update ancient versions of Alpine and CLI tools to modern ones.

OpenAPI & docs

- [x] Convert the remaining direct client database accesses in falconeri into REST API calls.
- [x] Generate OpenAPI specs for the REST API.
- [x] Add `falconeri schema` which outputs pipeline schema using schemars as formatted JSON.
- [x] Generate PDF version of guide as CI build artifact? mdbook-typst?
- [ ] There's a GitHub-compatible diagramming tool that can be used inline in Markdown (Mermaid). We might want to convert our existing diagrams to that format and configure mdbook appropriately?
- [ ] Kubernetes architecture diagram, including where secrets and config come from.
- [ ] Clean up comments on schema types to be user-facing; they're awful. And add examples for pipeline types.
