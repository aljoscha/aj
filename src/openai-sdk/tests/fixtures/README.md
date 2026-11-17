# llama.cpp Responses captures

Captured unmodified SSE bodies on 2026-11-17 from
`https://llm.tailfb5ac6.ts.net/v1/responses`, without Authorization.
Both requests used `Content-Type: application/json` and completed successfully.
The synthetic tool call was not executed.

Text request (`llama-cpp-text.sse`):

```json
{"model":"qwen3.8-27b","input":"Reply with exactly hello.","stream":true,"max_output_tokens":256}
```

Tool request (`llama-cpp-tool.sse`):

```json
{"model":"qwen3.8-27b","input":"Call lookup_weather for Berlin. Do not answer directly.","tools":[{"type":"function","name":"lookup_weather","description":"Synthetic read-only weather lookup. This probe does not execute the tool.","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}}],"tool_choice":"required","stream":true,"max_output_tokens":512}
```

Neither stream includes sequence numbers. Initial lifecycle responses contain
only `id`, `object`, and `status`. Completed responses also omit `tools` and
`parallel_tool_calls`. Reasoning arrives as `response.reasoning_text.delta`
and reasoning-item `content`, with empty `summary` and `encrypted_content`.
Usage has no `output_tokens_details`. The tool stream finishes its arguments
through `response.output_item.done`, without a function-arguments-done event.
