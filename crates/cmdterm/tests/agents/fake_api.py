#!/usr/bin/env python3
"""A stand-in for the model APIs the real agents call, for the agent harness.

It answers the Anthropic Messages API (Claude Code), the OpenAI Responses API
(Codex) and the OpenAI Chat Completions API (Qwen Code), streaming or not.
Every reply names how many user turns the conversation it got holds and what
the last one said, such as `HARNESS-REPLY turn=2 to=TASK second`. A resumed
session that kept its history answers its next prompt with turn=2, a new one
with turn=1.

Requests whose newest user message holds no prompt are marked "side": the
agent's own calls, such as titles, summaries or memory updates.

Every request is appended to $HARNESS_STATE/requests.jsonl, so a test can
check which prompts reached the model and how often. Only lines with TASK in them count as prompts (or the app's prompt to
continue), since agents add context of their own to user messages. A prompt
with SLOW in it
waits $HARNESS_SLOW seconds (default 600) before the reply starts, which keeps
the agent busy in the middle of a turn.

A prompt with `RUN: COMMAND` in it gets a call of the agent's shell tool
with COMMAND, the way a model asks an agent to run a command. The request
that carries the tool's result gets `HARNESS-RAN` back, ending the turn, and
its line in requests.jsonl holds the output as "tool_output".
"""

import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

STATE = os.environ.get("HARNESS_STATE", "/harness-state")
SLOW = float(os.environ.get("HARNESS_SLOW", "600"))
PORT = int(os.environ.get("HARNESS_PORT", "8787"))
LOCK = threading.Lock()
COUNTER = [0]


def record(entry):
    with LOCK:
        COUNTER[0] += 1
        entry["seq"] = COUNTER[0]
        entry["time"] = time.time()
        os.makedirs(STATE, exist_ok=True)
        with open(os.path.join(STATE, "requests.jsonl"), "a") as log:
            log.write(json.dumps(entry) + "\n")


def texts(content):
    """The plain texts of a message's content, in any of the APIs' shapes."""
    if isinstance(content, str):
        return [content]
    out = []
    for part in content or []:
        if isinstance(part, str):
            out.append(part)
        elif isinstance(part, dict) and part.get("type") in ("text", "input_text"):
            out.append(part.get("text", ""))
    return out


# What the harness types: prompts with TASK in them, and the app's prompt
# to continue. Agents wrap them in context of their own, in the same user
# message or in extra ones, so only lines with these count.
MARKERS = ("TASK", "Continue where you left off")


def prompt_in(text):
    for line in text.splitlines():
        if any(marker in line for marker in MARKERS):
            return line.strip()
    return None


def user_turns(messages):
    """The prompts the user typed, oldest first."""
    prompts = []
    for message in messages:
        if message.get("role") != "user":
            continue
        if message.get("type") not in (None, "message"):
            continue
        found = [p for p in map(prompt_in, texts(message.get("content"))) if p]
        if found:
            prompts.append(found[-1])
    return prompts


def run_command(prompt):
    """The command of a `RUN:` prompt, or None."""
    _, found, command = prompt.partition("RUN: ")
    return command.strip() if found else None


def tool_text(content):
    """The text of a tool's result, in any of the APIs' shapes."""
    if isinstance(content, str):
        return content
    out = []
    for part in content or []:
        if isinstance(part, str):
            out.append(part)
        elif isinstance(part, dict):
            out.append(tool_text(part.get("text") or part.get("content") or part.get("output")))
    return "\n".join(out)


def tool_result(messages):
    """The output of the tool call the newest messages answer, or None.
    Agents may add messages of their own after the result, such as
    reminders, so this looks back to the last message of the model."""
    for message in reversed(messages or []):
        if message.get("role") == "tool":
            return tool_text(message.get("content"))
        if message.get("type") in ("function_call_output", "local_shell_call_output",
                                   "custom_tool_call_output"):
            return tool_text(message.get("output"))
        if message.get("role") == "user" and isinstance(message.get("content"), list):
            results = [p for p in message["content"]
                       if isinstance(p, dict) and p.get("type") == "tool_result"]
            if results:
                return tool_text(results[-1].get("content"))
        if message.get("role") == "assistant" or message.get("type") in (
                "function_call", "local_shell_call", "reasoning"):
            return None
        if message.get("role") == "user" and any(map(prompt_in, texts(message.get("content")))):
            return None
    return None


def ran(api, messages, output):
    prompts = user_turns(messages)
    last = prompts[-1] if prompts else ""
    record({"api": api, "prompt": last, "turns": len(prompts), "side": False,
            "tool_output": output, "prompts": [prompt[:200] for prompt in prompts]})
    return f"HARNESS-RAN turn={len(prompts)}"


def wants_tool(messages, side):
    """The command the newest prompt asks to run, unless it already ran."""
    if side or tool_result(messages) is not None:
        return None
    users = [m for m in messages if m.get("role") == "user"]
    if not users:
        return None
    found = [p for p in map(prompt_in, texts(users[-1].get("content"))) if p]
    return run_command(found[-1]) if found else None


def reply_for(api, messages, side):
    prompts = user_turns(messages)
    # A request whose newest user message holds no prompt is the agent's
    # own, such as a summary or a memory update, not a turn of the user.
    users = [m for m in messages if m.get("role") == "user"]
    if not users or not any(map(prompt_in, texts(users[-1].get("content")))):
        side = True
    last = prompts[-1] if prompts else ""
    record({"api": api, "prompt": last, "turns": len(prompts), "side": side,
            "prompts": [prompt[:200] for prompt in prompts]})
    if "SLOW" in last and not side:
        time.sleep(SLOW)
    return f"HARNESS-REPLY turn={len(prompts)} to={last[:60]}"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def body(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            return json.loads(raw or b"{}")
        except ValueError:
            return {}

    def json(self, value, status=200):
        data = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def start_stream(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True

    def event(self, name, value):
        if name:
            self.wfile.write(f"event: {name}\n".encode())
        payload = value if isinstance(value, str) else json.dumps(value)
        self.wfile.write(f"data: {payload}\n\n".encode())
        self.wfile.flush()

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            return self.json({"object": "list", "data": [{"id": "harness-model", "object": "model"}]})
        self.json({})

    def do_HEAD(self):
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_POST(self):
        path = self.path.split("?")[0].rstrip("/")
        request = self.body()
        if path.endswith("/messages/count_tokens"):
            return self.json({"input_tokens": 10})
        if path.endswith("/messages"):
            return self.anthropic(request)
        if path.endswith("/responses"):
            return self.openai_responses(request)
        if path.endswith("/chat/completions"):
            return self.chat(request)
        self.json({})

    def anthropic(self, request):
        # The main conversation has tools; title and topic calls have none.
        side = not request.get("tools")
        messages = request.get("messages", [])
        output = tool_result(messages)
        command = wants_tool(messages, side)
        if output is not None and not side:
            text = ran("anthropic", messages, output)
        elif command is not None:
            return self.anthropic_tool(request, messages, command)
        else:
            text = reply_for("anthropic", messages, side)
        model = request.get("model", "harness-model")
        message = {
            "id": f"msg_harness_{COUNTER[0]}",
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [],
            "stop_reason": None,
            "stop_sequence": None,
            "usage": {"input_tokens": 10, "output_tokens": 1},
        }
        if not request.get("stream"):
            message["content"] = [{"type": "text", "text": text}]
            message["stop_reason"] = "end_turn"
            return self.json(message)
        self.start_stream()
        self.event("message_start", {"type": "message_start", "message": message})
        self.event("content_block_start", {"type": "content_block_start", "index": 0,
                                           "content_block": {"type": "text", "text": ""}})
        self.event("content_block_delta", {"type": "content_block_delta", "index": 0,
                                           "delta": {"type": "text_delta", "text": text}})
        self.event("content_block_stop", {"type": "content_block_stop", "index": 0})
        self.event("message_delta", {"type": "message_delta",
                                     "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                                     "usage": {"output_tokens": 5}})
        self.event("message_stop", {"type": "message_stop"})

    def anthropic_tool(self, request, messages, command):
        record({"api": "anthropic", "prompt": user_turns(messages)[-1], "side": False,
                "turns": len(user_turns(messages)), "command": command})
        call = {"type": "tool_use", "id": f"toolu_harness_{COUNTER[0]}", "name": "Bash",
                "input": {"command": command, "description": "Run the harness command",
                          "timeout": 600000}}
        message = {
            "id": f"msg_harness_{COUNTER[0]}",
            "type": "message",
            "role": "assistant",
            "model": request.get("model", "harness-model"),
            "content": [],
            "stop_reason": None,
            "stop_sequence": None,
            "usage": {"input_tokens": 10, "output_tokens": 1},
        }
        if not request.get("stream"):
            message["content"] = [call]
            message["stop_reason"] = "tool_use"
            return self.json(message)
        self.start_stream()
        self.event("message_start", {"type": "message_start", "message": message})
        self.event("content_block_start", {"type": "content_block_start", "index": 0,
                                           "content_block": dict(call, input={})})
        self.event("content_block_delta", {"type": "content_block_delta", "index": 0,
                                           "delta": {"type": "input_json_delta",
                                                     "partial_json": json.dumps(call["input"])}})
        self.event("content_block_stop", {"type": "content_block_stop", "index": 0})
        self.event("message_delta", {"type": "message_delta",
                                     "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                                     "usage": {"output_tokens": 5}})
        self.event("message_stop", {"type": "message_stop"})

    def openai_responses(self, request):
        items = request.get("input", [])
        if isinstance(items, str):
            items = [{"role": "user", "content": items}]
        # Codex names a thread with a request that has no tools.
        side = not request.get("tools")
        output = tool_result(items)
        command = wants_tool(items, side)
        if output is not None and not side:
            text = ran("responses", items, output)
        elif command is not None:
            record({"api": "responses", "prompt": user_turns(items)[-1], "side": False,
                    "turns": len(user_turns(items)), "command": command})
            text = None
        else:
            text = reply_for("responses", items, side)
        rid = f"resp_harness_{COUNTER[0]}"
        if text is None:
            item = shell_call(request.get("tools", []), command, rid)
        else:
            item = {"type": "message", "role": "assistant", "id": f"msg_{rid}",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": text, "annotations": []}]}
        usage = {"input_tokens": 10, "input_tokens_details": {"cached_tokens": 0},
                 "output_tokens": 5, "output_tokens_details": {"reasoning_tokens": 0},
                 "total_tokens": 15}
        if not request.get("stream"):
            return self.json({"id": rid, "object": "response", "status": "completed",
                              "output": [item], "usage": usage})
        self.start_stream()
        self.event("response.created", {"type": "response.created",
                                        "response": {"id": rid, "status": "in_progress"}})
        self.event("response.output_item.added", {"type": "response.output_item.added",
                                                  "output_index": 0,
                                                  "item": dict(item, status="in_progress",
                                                               **({"content": []} if text is not None else {}))})
        if text is not None:
            self.event("response.output_text.delta", {"type": "response.output_text.delta",
                                                      "item_id": item["id"], "output_index": 0,
                                                      "content_index": 0, "delta": text})
        self.event("response.output_item.done", {"type": "response.output_item.done",
                                                 "output_index": 0, "item": item})
        self.event("response.completed", {"type": "response.completed",
                                          "response": {"id": rid, "status": "completed",
                                                       "output": [item], "usage": usage}})

    def chat(self, request):
        messages = request.get("messages", [])
        side = not request.get("tools")
        output = tool_result(messages)
        command = wants_tool(messages, side)
        if output is not None and not side:
            text = ran("chat", messages, output)
        elif command is not None:
            record({"api": "chat", "prompt": user_turns(messages)[-1], "side": False,
                    "turns": len(user_turns(messages)), "command": command})
            return self.chat_tool(request, command)
        else:
            text = reply_for("chat", messages, side)
        cid = f"chatcmpl_harness_{COUNTER[0]}"
        model = request.get("model", "harness-model")
        usage = {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        if not request.get("stream"):
            return self.json({"id": cid, "object": "chat.completion", "created": int(time.time()),
                              "model": model, "usage": usage,
                              "choices": [{"index": 0, "finish_reason": "stop",
                                           "message": {"role": "assistant", "content": text}}]})
        self.start_stream()
        chunk = {"id": cid, "object": "chat.completion.chunk", "created": int(time.time()),
                 "model": model}
        self.event(None, dict(chunk, choices=[{"index": 0, "finish_reason": None,
                                               "delta": {"role": "assistant", "content": text}}]))
        self.event(None, dict(chunk, usage=usage,
                              choices=[{"index": 0, "finish_reason": "stop", "delta": {}}]))
        self.event(None, "[DONE]")

    def chat_tool(self, request, command):
        cid = f"chatcmpl_harness_{COUNTER[0]}"
        model = request.get("model", "harness-model")
        usage = {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        names = [t.get("function", {}).get("name", "") for t in request.get("tools", [])]
        name = next((n for n in names if "shell" in n), "run_shell_command")
        call = {"index": 0, "id": f"call_harness_{COUNTER[0]}", "type": "function",
                "function": {"name": name, "arguments": json.dumps(
                    {"command": command, "description": "Run the harness command"})}}
        if not request.get("stream"):
            message = {"role": "assistant", "content": None,
                       "tool_calls": [{k: v for k, v in call.items() if k != "index"}]}
            return self.json({"id": cid, "object": "chat.completion", "created": int(time.time()),
                              "model": model, "usage": usage,
                              "choices": [{"index": 0, "finish_reason": "tool_calls",
                                           "message": message}]})
        self.start_stream()
        chunk = {"id": cid, "object": "chat.completion.chunk", "created": int(time.time()),
                 "model": model}
        self.event(None, dict(chunk, choices=[{"index": 0, "finish_reason": None,
                                               "delta": {"role": "assistant", "content": None,
                                                         "tool_calls": [call]}}]))
        self.event(None, dict(chunk, usage=usage,
                              choices=[{"index": 0, "finish_reason": "tool_calls", "delta": {}}]))
        self.event(None, "[DONE]")


def shell_call(tools, command, rid):
    """A Responses API output item that calls the request's shell tool with
    `command`, shaped after the tool's parameters."""
    def params(tool):
        return (tool.get("parameters") or {}).get("properties") or {}
    by_name = {t.get("name") or t.get("type"): t for t in tools}
    if "local_shell" in by_name and by_name["local_shell"].get("type") == "local_shell":
        return {"type": "local_shell_call", "id": f"lsh_{rid}", "call_id": f"call_{rid}",
                "status": "completed",
                "action": {"type": "exec", "command": ["bash", "-lc", command], "env": {}}}
    for name in ("shell_command", "shell", "exec_command", "container.exec"):
        tool = by_name.get(name)
        if not tool:
            continue
        props = params(tool)
        if "cmd" in props:
            arguments = {"cmd": command}
        elif props.get("command", {}).get("type") == "array":
            arguments = {"command": ["bash", "-lc", command]}
        else:
            arguments = {"command": command}
        if "timeout_ms" in props:
            arguments["timeout_ms"] = 600000
        # exec_command returns after this long with the command still
        # running; npm alone takes seconds to start.
        if "yield_time_ms" in props:
            arguments["yield_time_ms"] = 120000
        return {"type": "function_call", "id": f"fc_{rid}", "call_id": f"call_{rid}",
                "status": "completed", "name": name, "arguments": json.dumps(arguments)}
    raise ValueError(f"no shell tool among {list(by_name)}")


if __name__ == "__main__":
    server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    print(f"fake API on 127.0.0.1:{PORT}, state in {STATE}", file=sys.stderr, flush=True)
    server.serve_forever()
