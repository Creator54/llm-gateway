# Plan: Implement Gemini API Provider for llm-gateway

## Overview

Add a complete `gemini-api` provider to the Rust gateway that handles Gemini API v1beta requests and streams. This provider integrates with the gateway's protocol translation layer, pricing/model routing, and cache-aware token accounting.

## Global Constraints

- **API Base:** https://generativelanguage.googleapis.com/v1beta/models
- **Auth Header:** x-goog-api-key
- **Streaming Endpoint:** :streamGenerateContent
- **Supported Protocols:** Chat (OpenAI format), Messages (Anthropic format), Responses (gateway format)
- **Models:** Seed from config (gemini-2.0-flash, gemini-2.0-pro, etc.) — no free-tier models
- **Error Mapping:**
  - 401/403 → Auth
  - 400 → Fatal 400
  - 429 → RateLimited
  - 500+ → Retryable
- **Cache Handling:** Relay x-goog-cache-* headers from upstream
- **Token Accounting:** Parse usageMetadata from response (input, cached_input, output)
- **Reference Implementation:** Use openai_api.rs structure and patterns
- **Shared Functions:** Use crate::translate for protocol conversion

## Tasks

### Task 1: Provider Adapter Core (gw/src/providers/gemini_api.rs)
Implement the base provider module with:
- GeminiApiProvider struct implementing the Provider trait
- Request building for Gemini API v1beta endpoint
- Response parsing and streaming support
- Error classification (401/403/400/429/500+)
- UsageMetadata token parsing (input, cached_input, output)
- Cache header relay (x-goog-cache-*)
- Passthrough for Chat → Chat, Messages → Messages, Responses → Responses

**Acceptance Criteria:**
- Module compiles without warnings
- Tests pass: basic request/response, streaming chunks, error classification, token parsing, cache headers
- Uses translate:: functions for protocol conversion
- Follows openai_api.rs structure

### Task 2: CLI Registration & Discovery
Add Gemini API provider to CLI infrastructure:
- Register in providers/mod.rs 
- Implement account add flow (API key prompt)
- Implement account list display
- Implement provider discovery endpoint
- Update CLI command routing

**Acceptance Criteria:**
- `gw account add gemini-api` prompts for and stores API key
- `gw account list` shows Gemini API accounts
- `gw provider discover gemini-api` returns supported models
- Credentials stored in ~/.config/gw/credentials.json (0600)
- Tests pass for credential flow and discovery

### Task 3: Pricing & Model Routing
Add pricing data and model routing:
- Define supported models list (gemini-2.0-flash, gemini-2.0-pro, gemini-1.5-flash, gemini-1.5-pro)
- Implement model → endpoint mapping
- Add pricing configuration (input/cached_input/output rates)
- Integrate with router for model selection
- Support model-specific features (flash vs pro capabilities)

**Acceptance Criteria:**
- Model list hard-coded in provider (no free-tier models)
- Routing maps model name to Gemini endpoint correctly
- Pricing applied during token accounting
- Tests verify model mapping and pricing calculation
- Router correctly selects gemini-api provider by model name

### Task 4: Full Test Suite
Comprehensive integration and unit tests:
- Streaming request/response handling
- Error handling (all 5 error classes + parsing failures)
- Cache-friendly usage parsing (edge cases: missing fields, malformed metadata)
- Token accounting (input, cached_input, output combinations)
- Protocol translation (Chat/Messages/Responses)
- Request building (headers, payload structure)
- Cache header relay
- Multi-chunk streaming and backpressure
- Mocking upstream Gemini API responses

**Acceptance Criteria:**
- Coverage ≥ 85% on gemini_api.rs
- All test categories pass
- Streaming tests verify correct chunk ordering and no data loss
- Error tests verify correct classification and retry behavior
- Token parsing tests cover edge cases
- Integration tests show end-to-end protocol translation

## Execution Flow

1. Task 1 implemented and tested inline
2. Task 1 reviewed and committed
3. Task 2 implemented and tested inline
4. Task 2 reviewed and committed
5. Task 3 implemented and tested inline
6. Task 3 reviewed and committed
7. Task 4 implemented and tested inline
8. Task 4 reviewed and committed
9. Final whole-branch review

## Success Metrics

- All tasks complete with clean reviews
- Gateway accepts Gemini API credentials and routes requests
- Streaming works without buffering
- Token accounting correctly reflects cached tokens
- Error handling matches contract (401/403/400/429/500+)
- Tests run in <5s and have ≥85% coverage
