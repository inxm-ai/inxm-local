# compiler module

The only place an LLM is invoked. Turns chat intent into a typed Plan.
The executor never calls back into this module at runtime.

Files:
- `backend.rs` — request types and the profile-backed `Backend` (transport lives in `src/llm.rs`)
- `config.rs` — backend config
- `extractor.rs` — pulls plan JSON out of raw LLM output
- `prompt.rs` — prompt templates

You own this dir only. Read other `src/` dirs for context, never edit them.
