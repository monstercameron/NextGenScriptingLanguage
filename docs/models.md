# Model operations

Model calls are explicit native applet calls. The bundled `execute-model`
helper reads the program's current pipeline bytes on stdin and writes one
normalized JSON result to stdout. Source builds must build both binaries with
`cargo build --release`; the helper must sit beside `execute`. The ordinary
`execute` runtime supervises it, including process-group cancellation and the
outer command timeout and output-byte limit. Model calls are never cached and
are never retried automatically. They do not change program control flow: parse
the result and use `.program` conditions to choose the next steps. Input bytes
that are valid JSON strings, objects, or arrays are sent with that type; other
input, including JSON numbers, booleans, and null, is sent as UTF-8 text.

Every successful result has `protocol: "execute.model/1"`, `provider`, `model`,
`kind`, `transport`, and a `usage` object with nonnegative integer
`input_tokens`, `output_tokens`, and `total_tokens`. `transport` is `https` for a
provider request and `fixture` for offline fixture mode. Explicit loopback HTTP test endpoints use
`http_loopback`; ordinary provider endpoints require HTTPS. The operation-specific
fields are:

| Applet | `kind` | Result fields |
| --- | --- | --- |
| `jev.classify` | `classify` | `label` (text), `confidence_percent` (integer 0–100), `accepted` (boolean) |
| `jev.judge` | `judge` | `decision` (boolean), `confidence_percent` (integer 0–100), `accepted` (boolean) |
| `jev.score` | `score` | `score` (number), `confidence_percent` (integer 0–100), `accepted` (boolean) |
| `llm.ask` | `generate` | `text` (text) |
| `llm.extract` | `extract` | `value` (JSON) |

`confidence_percent` floors the provider confidence multiplied by 100;
`accepted` compares that integer with `minimum`. It does not indicate a positive
label or yes decision. An omitted `minimum` uses the helper's default threshold of 0. A well-formed
result below the threshold succeeds with `accepted: false`; invalid provider
responses fail the command instead of looking like a low-confidence result.

The applets are declared in the manifest catalog as `jev.classify`, `jev.judge`,
`jev.score`, `llm.ask`, and `llm.extract`. `jev.*` uses TypeSafe; `llm.*` uses
OpenAI. The provider and operation are fixed by the selected applet. No shell
interpolation or hidden HTTP call is performed by the `execute` runtime.

## Classification

Pass a question and a closed list of at least two labels. An optional `minimum`
is a confidence threshold from 0 to 100. The response includes the selected `label`,
`confidence_percent`, and `accepted`; branch on `accepted` before acting on the
label:

```text
accept message as text
run "printf" with arguments ["%s", message]
then jev.classify with question "Which team should handle this message?" and labels ["billing", "account", "other"] and minimum 75 as raw
then parse json as result
take boolean at "/accepted" from result as accepted
take text at "/label" from result as label
if accepted is true
  if label is "billing"
    then "printf" with arguments ["Route to billing\n"]
  otherwise
    then "printf" with arguments ["Route to the general queue\n"]
otherwise
  then "printf" with arguments ["Send for human review\n"]
```

The label list is the allowed output set. The helper rejects a response whose
label is outside it; labels beginning with `--` are rejected because labels and
score levels are passed as explicit argv items. Supply descriptions in the
question text; each label is also used as its choice criterion. `jev.judge`
takes `question` and returns a boolean `decision`; confidence is the higher of
the yes and no probabilities. `jev.score` takes `question` and an ordered
`levels` list with at least two levels, then returns a numeric expected score,
which may be fractional.
Both include confidence and the threshold-derived `accepted` field. The default
`minimum` is 0, so set a threshold when low-confidence results should go to
review. The Jev default model is `jev-latest`; OpenAI calls require the caller to
name a model. The normalized `model` field contains the model returned by the
provider.

## Text generation and extraction

`execute model --help` exposes the same helper interface for standalone Unix
pipelines; named manifests provide the `.program` parameter contracts.

`llm.ask` requires an explicit OpenAI `model` and `instruction`. Its output
contains generated `text`. `llm.extract` also requires a JSON Schema string and
returns the extracted `value` as JSON. Keep schemas in a text file and read it
into a binding when that is easier to maintain:

```text
accept source as text
read "extraction-schema.json" as schema
run "printf" with arguments ["%s", source]
then llm.extract with model "<model-id>" and instruction "Extract the invoice fields." and schema schema as raw
then parse json as result
take text at "/value/vendor" from result as vendor
take integer at "/value/amount_cents" from result as amount
```

OpenAI structured extraction uses strict JSON Schema output. The helper accepts
these schema keywords: `$schema`, `$id`, `$comment`, `title`, `description`,
`examples`, `default`, `deprecated`, `readOnly`, `writeOnly`, `type`, `enum`,
`const`, `required`, `properties`, `additionalProperties`, `items`, `minLength`,
`maxLength`, `minItems`, `maxItems`, `minimum`, `maximum`, `exclusiveMinimum`,
`exclusiveMaximum`, `multipleOf`, `minProperties`, `maxProperties`,
`uniqueItems`, and `anyOf`. The root schema must be an object. Nested `type` declarations may use a
nonempty list of distinct supported types, such as `["integer", "null"]`, to
represent optional values while keeping every property required. Object schemas must set `additionalProperties` to
`false` and list every declared property in `required`. Other JSON Schema
keywords are rejected before the request. The helper checks the returned value
against the schema. Integer JSON values within the signed/unsigned 64-bit range
retain their precision. Fractional and exponent values use the runtime's finite
floating-point JSON representation and can round during parsing; this is not
arbitrary-precision decimal storage. Bounds, enum/const equality, uniqueness and
`multipleOf` use the normalized represented number. Use integer units or text
when a value must preserve every source decimal digit. Refusals, incomplete output, provider errors, invalid JSON,
and schema failures are errors; they do not become a low-confidence result or
trigger a program branch. See the [OpenAI Structured Outputs guide](https://developers.openai.com/api/docs/guides/structured-outputs?api-mode=responses).

## Endpoints, local models, and generation controls

`llm.ask` and `llm.extract` accept these optional parameters. Each one is sent
only when written, so a program that omits them produces the same request as
before:

| Parameter | Values | Wire effect |
| --- | --- | --- |
| `endpoint` | URL | Responses: the full URL, default `https://api.openai.com/v1/responses`. Chat Completions: a base URL; the helper posts to `{base}/chat/completions`, default base `https://api.openai.com/v1`. A trailing slash on the base is removed. |
| `api_key_env` | `EXECUTE_MODEL_KEY_` followed by a name, such as `EXECUTE_MODEL_KEY_LOCAL` | The only key a custom endpoint can receive. The prefix is required and case-sensitive. See the credential rule below. |
| `api` | `responses` (default), `chat_completions` | Selects the OpenAI wire protocol. |
| `effort` | `none`, `low`, `medium`, `high`, `xhigh`, `max` | Responses: `reasoning.effort`. Chat Completions: `reasoning_effort`. `minimal` and other values are rejected locally. |
| `temperature` | number from 0 to 2 | Top-level `temperature` in both protocols. Write a JSON number, not text. |

Chat Completions sends `messages` with the instruction as `system` and the
state as `user`, `stream: false`, and `max_tokens` (512 unless `max_tokens` is
given). Extraction sends `response_format` with a strict `json_schema`. The
result reads `choices[0].message.content`, `usage.prompt_tokens`, and
`usage.completion_tokens`. A `finish_reason` of `length` is `incomplete`;
`content_filter` or a non-null `message.refusal` is `refusal`. A missing or
inconsistent `usage`, multiple choices, tool calls, or an unsupported role fail
as `provider_output`. Local servers must therefore return usage.

Compatible local servers such as llama.cpp, Ollama and vLLM expose Chat
Completions. Example:

```text
llm.ask with model "local-model" and instruction "Summarize." and endpoint "http://127.0.0.1:11434/v1" and api "chat_completions" and effort "low" and temperature 0.2 and fixture "fixture.json" as raw
```

Endpoint rules: HTTPS is required for remote hosts. Plain HTTP is accepted only
for loopback hosts (`127.0.0.1`, `::1`, `localhost`). Userinfo, query strings,
and fragments are rejected. Redirects are never followed. Proxy environment
variables are ignored; the explicit endpoint is the only destination. Live
loopback endpoints additionally require `EXECUTE_MODEL_ALLOW_LOOPBACK` in the
helper's environment (presence enables it); without it the request fails as
configuration before any credential read or connection. Filtered child
environments (`--clear-env`, `--env-allow`, or a capability-policy environment
grant) must list the variable explicitly. Fixture runs never open a socket and
do not need it. Model endpoints accept no `network_origins` grant: dependency
#375 keeps them outside that policy, so this variable is the interim explicit
grant until that work lands.

Credential rule, which is the security boundary for provider keys:

- Default provider keys (`OPENAI_API_KEY`, `openai`, `JEV_API_KEY`,
  `TYPESAFE_API_KEY`, `jev`) are attached only to the provider's own host on
  the default port: `api.openai.com` for OpenAI and `api.typesafe.ai` for Jev.
  That is true whether the key comes from the process environment or an
  `env_file`. A lookalike host such as `api.openai.com.example.test` does not
  qualify.
- `api_key_env NAME` names one explicit variable. `NAME` must start with the
  dedicated prefix `EXECUTE_MODEL_KEY_` followed by at least one character, for
  example `EXECUTE_MODEL_KEY_LOCAL` or `EXECUTE_MODEL_KEY_OPENROUTER`. The check
  is case-sensitive and happens at parse time, so `GITHUB_TOKEN`,
  `AWS_SECRET_ACCESS_KEY`, `execute_model_key_local`, and `OPENAI_API_KEY` are all
  rejected before any file read or connection. The helper reads the variable
  from the process environment, or from the explicit `env_file` if the process has
  no value. The `env_file` lookup follows the same prefix rule, so a file can
  supply only prefixed names through this parameter. The named key may be sent to
  any endpoint, so put only the key intended for that endpoint under the prefix.
- A custom endpoint without `api_key_env` gets no `Authorization` header. This
  covers loopback servers and remote hosts. A remote custom host therefore
  proceeds without a key, which is correct for open hosts. It never receives the
  default keys.
- On the default host with no default key, the request fails as
  `credentials_missing` before any connection. An `api_key_env` that names an
  unset variable also fails as `credentials_missing` before connecting.

Process environment values take precedence over an `env_file` for the same name.
Use `env_file` for the default keys on the default host, and `api_key_env` with an
`EXECUTE_MODEL_KEY_` name for any other key.

`api`, `effort` and `temperature` are OpenAI-only. Jev rejects them, and the
helper rejects a duplicate of any of these options. Fixtures record `api`,
`effort`, and `temperature` in the semantic request only when those options are
used. `endpoint` is transport, so it is never part of the fixture request, which
lets one fixture serve any loopback port. A fixture that describes
`chat_completions` must include `"api": "chat_completions"`. Integer temperature
values are stored as decimals, so write `1.0` in a fixture rather than `1`.

## Limits, credentials, and offline fixtures

The manifests expose `timeout_ms` (1–300,000), `max_bytes` (1–16,777,216), and
optional `fixture` for each operation; Jev operations also accept `minimum`
(0–100) and optional `model`. OpenAI operations accept `max_tokens`
(1–65,536) and require an explicit `model`. `llm.ask` and
`llm.extract` also accept `endpoint`, `api`, `effort`, and `temperature`; see
the next section. `timeout_ms`
bounds the helper's HTTP request. The runtime's `--command-timeout-ms` bounds the
whole native pipeline, including helper startup and cleanup. `max_bytes` bounds
the helper's input and provider request/response; the runtime's
`--max-output-bytes` separately limits captured pipeline output. OpenAI requests default to an output token cap of 512; set `max_tokens` to
change it. Provider token limits and byte limits measure different things.

Model names and required question or instruction text must contain non-whitespace
text. The manifests reject empty and whitespace-only literals during `execute
check`, and validate referenced strings after they resolve at runtime.

Pass credentials through a private `env_file` parameter or the helper process
environment. The environment file accepts `jev=...` for TypeSafe or
`openai=...` for OpenAI; uppercase `JEV_API_KEY`, `TYPESAFE_API_KEY`, and
`OPENAI_API_KEY` are also accepted. Process environment values take precedence
over values in the file. Keep secrets out of `.program` source and manifests.
The helper emits sanitized error classes without provider response bodies,
request values, paths, or credentials.

Fixtures allow deterministic offline operation and validation without
contacting a provider. A fixture checks the complete semantic request, including
input state and operation settings, before supplying a provider-native response;
this exercises the same response validation as a live request. If `fixture` is
present, the helper must open and validate that file. An empty, missing, or
mismatched fixture is an error and never falls back to a live provider request.
Omit `fixture` for live calls. The presence of `EXECUTE_MODELS_OFFLINE` in the
process environment, even with an empty value, rejects every fixtureless call
before reading input, credentials, or opening a connection. Fixture calls still
work. The [100-workflow runner](../examples/models/README.md) always enables
this guard, sanitizes credentials and uses an OS network sandbox where available.

The workflow collection defaults to fixtures. Its explicit `live` boolean selects
a separate fixtureless branch; use `--input live=true` deliberately for real
inference. Offline fixture success tests transport contracts and program routing,
not model accuracy, calibration, or real generated-output quality.

With checkpointing, completed model results are restored without another
request. Checkpoint state stores model input and output in plaintext private
files, so restrict access to that directory. A failed model call may already
have reached the provider and incurred a charge; `uncertain_on_failure` makes
resume require `--retry-uncertain` before trying that node again. Use that retry
only when repeating the request is acceptable. There is no automatic retry.

TypeSafe's public API describes `POST /v1/systemone`, its `noul`, `choice`, and
`score` question types, and the corresponding answer fields. The helper maps
those provider-specific responses into the normalized result described above.
See the [TypeSafe OpenAPI contract](https://api.typesafe.ai/openapi.json). The
OpenAI adapter uses the Responses API with Structured Outputs for extraction.

## Current scope

The adapter implements one classification, judgment, score, generation, or
extraction per call. Batched named Jev questions, embeddings, streaming tokens,
provider failover, and automatic tool execution are future work. Model results
are data; executing any suggested command requires an explicit `.program` step.

The [validation receipt](model-integration-validation.json) records source hashes,
fixture and adversarial coverage, bounded live connectivity checks, and measured
offline startup overhead. Historical installed artifacts are unchanged.
