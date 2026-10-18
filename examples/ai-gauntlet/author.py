#!/usr/bin/env python3
"""Regenerate authored examples and fixtures; never contacts a provider.

Fixtures are synthetic contracts, not evidence of model accuracy. Run run.py
after regeneration to check the independent execution path against these oracles.
"""
from __future__ import annotations

import copy
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent


def compact(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8", newline="\n")


def choice(label, labels, confidence=0.95):
    rest = (1 - confidence) / (len(labels) - 1)
    return {"type": "choice", "choice": label, "confidence": confidence,
            "probabilities": {item: confidence if item == label else rest for item in labels}}


def noul(probability=0.95):
    return {"type": "noul", "noul": probability}


def score(value=1.9, confidence=0.95):
    if value == 1:
        probabilities = {"0": 0.025, "1": 0.95, "2": 0.025}
    elif confidence < 0.8:
        probabilities = {"0": 0.2, "1": 0.2, "2": 0.6} if value > 1 else {"0": 0.6, "1": 0.2, "2": 0.2}
        value = 1.4 if value > 1 else 0.6
    else:
        probabilities = {"0": 0.05, "1": 0, "2": 0.95} if value > 1 else {"0": 0.95, "1": 0, "2": 0.05}
    return {"type": "score", "score": value, "confidence": confidence,
            "legend": {"0": "low", "1": "medium", "2": "high"},
            "probabilities": probabilities}


def jev(answers):
    return {"model": "jev-latest", "answers": answers,
            "usage": {"input_tokens": 24, "output_tokens": 6}}


def openai(text, refused=False):
    content = {"type": "refusal", "refusal": "Cannot answer from this evidence."} if refused else {
        "type": "output_text", "text": text, "annotations": []}
    return {"id": "resp-fixture", "object": "response", "status": "completed", "model": "gpt-6-luna",
            "output": [{"id": "msg-fixture", "type": "message", "status": "completed",
                        "role": "assistant", "content": [content]}],
            "usage": {"input_tokens": 24, "output_tokens": 12, "total_tokens": 36}}


def schema(properties):
    return {"type": "object", "properties": properties, "required": list(properties),
            "additionalProperties": False}


TEXT = {"type": "string", "minLength": 1, "maxLength": 120}

# Short sources are deliberately ordinary desktop/developer data rather than
# incident-response exports. Labels are source-owned identifiers, not shell code.
SPECS = [
    dict(slug="log-errors", task="Classify a log line and summarize errors only", mode="generate", log_labels=True,
         source="cache: could not open index; rebuild required\n", question="Is this line an error that needs a short explanation, rather than a warning or routine noise?",
         instruction="Summarize the supplied error in one sentence. Do not invent a cause or suggest an action as certain.",
         answer="The cache index could not be opened; a rebuild is required according to the log.",
         bash="regex rules miss paraphrased failures; a separate summarizer and confidence gate are needed"),
    dict(slug="receipt-json", task="Extract a receipt into integer money units", mode="extract",
         source="Thanks Ada Chen! Paid North Books 24.50 USD on 2026-10-17.\n",
         question="Does this receipt explicitly provide a customer name, ISO date, USD amount and currency?",
         instruction="Extract the customer name, date, currency and amount in integer cents, using only the receipt.",
         schema=schema(dict(name=TEXT, date=TEXT, currency={"type": "string", "enum": ["USD"]}, amount_cents={"type": "integer", "minimum": 0, "maximum": 1000000})),
         value=dict(name="Ada Chen", date="2026-10-17", currency="USD", amount_cents=2450),
         bash="merchant-specific regex plus jq and integer-cents validation"),
    dict(slug="wifi-pick", task="Map a fuzzy Wi-Fi description to a known network", mode="classify",
         source={"request": "the private one upstairs, not the visitor hotspot", "networks": {"home": "upstairs private WPA network", "guest": "visitor hotspot", "unknown": "no match"}},
         question="Choose the matching supplied network ID; unknown if ambiguous. Never invent an SSID.", labels=["home", "guest", "unknown"], label="home", other="unknown",
         bash="hand-maintained synonym rules plus a closed ID allowlist"),
    dict(slug="audio-pick", task="Choose an audio device from a fuzzy description", mode="classify",
         source={"request": "use the cans with a microphone", "devices": {"headset": "USB headphones and microphone", "speakers": "built-in speakers", "unknown": "no match"}},
         question="Choose the supplied audio device ID matching the request; unknown if unclear.", labels=["headset", "speakers", "unknown"], label="headset", other="unknown",
         bash="device-specific aliases, regex, and a manual review branch"),
    dict(slug="config-risk", task="Review a configuration change before applying it", mode="judge",
         source="Proposed editor setting: format_on_save false -> true; scope: this workspace only.\n",
         question="Is this change limited to reversible editor formatting, with no permissions, network or deletion changes?",
         result="workspace formatting change eligible for human review",
         bash="a hand-written semantic policy over config keys and change context"),
    dict(slug="commit-message", task="Draft a commit message with explicit confirmation", mode="generate", confirm=True,
         source="diff --git a/ui.txt b/ui.txt\n-Recieve updates\n+Receive updates\n",
         question="Does this diff contain enough evidence for a small factual commit message?",
         instruction="Write one short imperative commit subject describing only the supplied diff. No shell command.",
         answer="Fix spelling of Receive in update label",
         bash="a generator client, JSON decoding, error handling and a separate confirmation gate"),
    dict(slug="stderr-explain", task="Explain saved stderr and show a diagnostic", mode="generate",
         source="$ cp report.txt archive/\ncp: cannot create regular file 'archive/report.txt': Permission denied\n",
         question="Does this saved stderr give a concrete failing command and error to explain without guessing?",
         instruction="Explain this stderr in one paragraph and show one read-only diagnostic command. Label it as a suggestion, never an instruction to execute automatically.",
         answer="The copy could not create the destination file because access was denied. Suggested diagnostic (review only): ls -ld archive",
         bash="stderr pattern rules plus a model client and an explicit display-only command boundary"),
    dict(slug="bug-duplicates", task="Identify near-duplicate bug reports without merging", mode="judge",
         source={"A": "Saving an empty draft closes the editor unexpectedly", "B": "App exits when I hit save on a blank draft"},
         question="Do these reports describe the same trigger and observed symptom, rather than merely the same component?",
         result={"duplicate_candidates": ["A", "B"], "merge": False},
         bash="fuzzy string metrics miss semantic paraphrases and need a review threshold"),
    dict(slug="todo-urgency", task="Score one TODO against an explicit deadline", mode="score",
         source="TODO: renew the signing certificate before tomorrow's demo; build signing expires tonight.\n",
         question="Rate urgency: low means cosmetic, medium means this week, high means blocking work within 24 hours.",
         result="review this TODO before routine cleanup", high=True,
         bash="deadline parsing, urgency heuristics and a calibrated review branch"),
    dict(slug="download-tag", task="Tag a downloaded document by its content", mode="classify",
         source="download_826.txt: Amount due 72 USD; invoice number INV-19; payment terms net 30.\n",
         question="Tag this file by content, using only invoice, notes, source, or unknown.", labels=["invoice", "notes", "source", "unknown"], label="invoice", other="unknown",
         bash="content regex per document type plus metadata and uncertainty handling"),
    dict(slug="manual-search", task="Find a man-page snippet from a natural-language request", mode="classify",
         source={"request": "show hidden entries too", "snippets": {"all": "ls -a: do not ignore entries starting with .", "long": "ls -l: use a long listing format", "unknown": "no match"}},
         question="Return the supplied snippet ID that answers the request; unknown if none does.", labels=["all", "long", "unknown"], label="all", other="unknown",
         bash="a synonym index to bridge user intent and man-page vocabulary"),
    dict(slug="command-danger", task="Refuse dangerous commands and gate a literal safe command", mode="score", unix_only=True, confirm=True,
         source="pwd", question="Rate command danger: low is read-only pwd; medium changes files; high deletes data, expands a shell, or changes privileges.",
         result="pwd", high=False,
         bash="a command allowlist, a model client, score parsing, threshold and confirmation plumbing"),
    dict(slug="notification-triage", task="Triage a notification into now, later or never", mode="ask",
         source="Your local build needs your approval before the demo in ten minutes.\n",
         questions={"route": {"type": "choice", "question": "When should the user review this notification?", "labels": ["now", "later", "never"], "minimum": 80}, "actionable": {"type": "noul", "question": "Does this notification ask the user for a concrete action?", "minimum": 80}},
         label="now", other="later", result="now",
         bash="priority keyword rules and two independently checked model answers"),
    dict(slug="journal-status", task="Summarize saved desktop journal output", mode="generate",
         source="10:00 music-player: library scan started\n10:01 music-player: scan complete, 120 tracks\n10:01 music-player: ready\n",
         question="Do these journal lines explicitly show the final state of the library scan?",
         instruction="Produce one status line grounded only in the supplied journal; preserve counts and do not infer system health.",
         answer="Music library scan complete: 120 tracks; player reported ready.",
         bash="application-specific log parsing plus a separate summarization client"),
    dict(slug="request-route", task="Turn a free-form request into a typed routed record", mode="extract",
         source="Please review docs/readme.md for about 15 minutes; do not edit it.\n",
         question="Does the request explicitly name a file, a review-only action and a bounded duration?",
         instruction="Extract a review-only request. Route must be docs or code; action must be review; duration is integer minutes.",
         schema=schema(dict(path=TEXT, route={"type": "string", "enum": ["docs", "code"]}, action={"type": "string", "enum": ["review"]}, minutes={"type": "integer", "minimum": 1, "maximum": 60})),
         value=dict(path="docs/readme.md", route="docs", action="review", minutes=15),
         bash="intent grammar, field regex, jq schema checks and explicit route validation"),
    dict(slug="appointment-record", task="Extract an appointment proposal without scheduling", mode="extract",
         source="Let's meet Sam at the library on 2026-10-22 at 14:30, America/New_York.\n",
         question="Are a participant, place, explicit date, time and time zone all present?",
         instruction="Extract only the explicitly stated appointment fields; do not schedule or resolve ambiguous dates.",
         schema=schema(dict(person=TEXT, place=TEXT, date=TEXT, time=TEXT, zone=TEXT)),
         value=dict(person="Sam", place="library", date="2026-10-22", time="14:30", zone="America/New_York"),
         bash="date/entity parsing, ambiguity checks and a calendar review boundary"),
    dict(slug="change-category", task="Classify a change description for a changelog", mode="classify",
         source="The search panel no longer crashes when the query contains an emoji.\n",
         question="Classify this change as fix, feature, docs, or unknown using its user-visible effect.", labels=["fix", "feature", "docs", "unknown"], label="fix", other="unknown",
         bash="fragile commit-prefix or keyword rules for semantic change categories"),
    dict(slug="download-duplicates", task="Review whether two download descriptions are duplicates", mode="judge",
         source={"a": "Rust shortcuts, edition 3, twelve pages", "b": "Third-edition Rust keyboard quick reference, 12 pp"},
         question="Do these descriptions plausibly refer to the same edition and document? This is a review candidate, not proof of byte equality.",
         result={"candidate_pair": ["a", "b"], "delete": False},
         bash="normalization and fuzzy matching still need semantic edition checks"),
    dict(slug="backup-exclusions", task="Review a proposed backup exclusion", mode="judge",
         source="Exclude only project/target/ compiler outputs. Keep project/src/, manifests and user documents.\n",
         question="Does this proposal exclude only regenerable build outputs while explicitly retaining source and user documents?",
         result="candidate exclusion: project/target/; verify with a dry run",
         bash="a directory-name policy misses contextual claims about what is regenerable"),
    dict(slug="keybinding-search", task="Find a keyboard shortcut from a fuzzy request", mode="classify",
         source={"request": "jump to the thing that defines this symbol", "bindings": {"definition": "F12: Go to definition", "references": "Shift-F12: Find references", "unknown": "no match"}},
         question="Which supplied binding ID best matches the request? Use unknown for ambiguity.", labels=["definition", "references", "unknown"], label="definition", other="unknown",
         bash="a synonym dictionary for intent rather than exact help text matches"),
    dict(slug="rename-proposal", task="Extract a file rename proposal without changing files", mode="extract",
         source="Rename the draft called notes-final-final.txt to meeting-notes.txt, please.\n",
         question="Does this request explicitly name an old and new basename, with no directory move?",
         instruction="Extract the old and new basenames and action propose. Never perform a rename.",
         schema=schema(dict(old=TEXT, new=TEXT, action={"type": "string", "enum": ["propose"]})),
         value=dict(old="notes-final-final.txt", new="meeting-notes.txt", action="propose"),
         bash="phrase variants, filename extraction and JSON escaping before a review step"),
    dict(slug="regex-explain", task="Explain a saved regular expression", mode="generate",
         source="Pattern: ^[A-Z]{2}-[0-9]{4}$; purpose: validate local ticket IDs.\n",
         question="Is there a complete literal pattern and an explicit validation purpose to explain?",
         instruction="Explain the supplied regex in plain language and give a matching example; do not generate or execute code.",
         answer="A ticket ID must be two uppercase ASCII letters, a hyphen, and four digits, with nothing before or after, for example AB-1234.",
         bash="a regex parser plus plain-language templates or a separate generation client"),
    dict(slug="todo-route", task="Route an actionable TODO with a batched judgement", mode="ask",
         source="Before lunch, ask Morgan to review the typo fix in the getting-started guide.\n",
         questions={"route": {"type": "choice", "question": "Which local review queue fits this TODO?", "labels": ["docs", "code", "unknown"], "minimum": 80}, "actionable": {"type": "noul", "question": "Does this TODO name a concrete next action rather than a vague aspiration?", "minimum": 80}},
         label="docs", other="unknown", result="docs",
         bash="a classifier client and an actionability client with separate acceptance checks"),
    dict(slug="help-summary", task="Explain a command option from saved help text", mode="generate",
         source="tool --dry-run: print proposed file changes without writing them. --apply: write the proposed file changes.\n",
         question="Does the supplied help explicitly distinguish preview from applying changes?",
         instruction="Explain which supplied option previews changes. Mention the applying option only to distinguish it; never execute either.",
         answer="Use --dry-run to preview proposed file changes. The help says --apply writes them, so review the preview first.",
         bash="intent matching against help plus a generation client and display-only output handling"),
]


def build():
    workflows = []
    for index, spec in enumerate(SPECS, 1):
        ordinal = f"{index:03}"
        root = HERE / "fixtures" / ordinal
        mode = spec["mode"]
        source = spec["source"]
        state = source
        source_file = "source.json" if isinstance(source, dict) else "source.txt"
        root.mkdir(parents=True, exist_ok=True)
        (root / source_file).write_text(compact(source) if isinstance(source, dict) else source, encoding="utf-8", newline="\n")
        fallback = dict(status="needs-human-review", result=None,
                        action="Read the original input; no action is authorized.")
        candidate = dict(status="candidate-for-review", result=spec.get("result", spec.get("label")),
                         action="Review against the original input before acting.")

        def output(value, binding=None):
            # json.set is in-process and serializes text as data, with no shell.
            document = dict(value, result=None)
            return f'run json.set with document {compact(document)} and pointer "/result" and value {binding if binding else compact(value["result"])}'

        fallback_line = output(fallback)
        lines = [f'# Purpose: {spec["task"]}.',
                 f'# Inputs: source_path, fixture paths, live=false' + (", confirmed=false." if spec.get("confirm") else "."),
                 f'# Outputs: out/{ordinal}.json; candidate only; model text is never command code.',
                 f'# Bash would need: {spec["bash"]}.',
                 f'accept source_path as text with default "fixtures/{ordinal}/{source_file}"',
                 f'accept fixture as text with default "fixtures/{ordinal}/confident.json"',
                 'accept live as boolean with default false']
        if spec.get("confirm"):
            lines.append('accept confirmed as boolean with default false')
        if mode in {"generate", "extract"}:
            lines.append(f'accept result_fixture as text with default "fixtures/{ordinal}/result.json"')
        if mode == "extract":
            lines.append(f'accept schema_path as text with default "fixtures/{ordinal}/schema.json"')
            write(root / "schema.json", spec["schema"])
        lines.extend(['try', '  read source_path'])
        expected = dict(provider="jev", kind=mode, model="jev-latest", state=state)
        if spec.get("log_labels"):
            labels = ["error", "warn", "noise"]
            call = f'jev.classify with question {compact(spec["question"])} and labels {compact(labels)} and minimum 80'
            expected.update(kind="classify", question=spec["question"], labels=labels, minimum=80)
            answer, low_answer, other_answer = choice("error", labels), choice("error", labels, 0.6), choice("warn", labels)
        elif mode == "classify":
            call = f'jev.classify with question {compact(spec["question"])} and labels {compact(spec["labels"])} and minimum 80'
            expected.update(question=spec["question"], labels=spec["labels"], minimum=80)
            answer = choice(spec["label"], spec["labels"])
            low_answer = choice(spec["label"], spec["labels"], 0.6)
            other_answer = choice(spec["other"], spec["labels"])
        elif mode == "ask":
            call = f'jev.ask with questions {compact(spec["questions"])}'
            expected["questions"] = spec["questions"]
            answers = dict(route=choice(spec["label"], spec["questions"]["route"]["labels"]), actionable=noul())
            response = jev(answers)
            low_response = jev(dict(route=choice(spec["label"], spec["questions"]["route"]["labels"], 0.6), actionable=noul()))
            other_response = jev(dict(route=choice(spec["label"], spec["questions"]["route"]["labels"]), actionable=noul(0.05)))
        elif mode == "score":
            call = f'jev.score with question {compact(spec["question"])} and levels ["low","medium","high"] and minimum 80'
            expected.update(question=spec["question"], levels=["low", "medium", "high"], minimum=80)
            answer = score(1.9 if spec["high"] else 0.1)
            low_answer = score(answer["score"], 0.6)
            other_answer = score(0.1 if spec["high"] else 1.9)
        else:
            call = f'jev.judge with question {compact(spec["question"])} and minimum 80'
            expected.update(kind="judge", question=spec["question"], minimum=80)
            answer, low_answer, other_answer = noul(), noul(0.6), noul(0.05)
        if mode != "ask":
            response, low_response, other_response = (jev({"decision": item}) for item in (answer, low_answer, other_answer))
        suffix = ' and timeout_ms 3000 and max_bytes 65536'
        lines.extend(['  if live is true', f'    then {call}{suffix} as raw', '  otherwise',
                      f'    then {call}{suffix} and fixture fixture as raw', '  then parse json as verdict'])
        prefix = "/answers/route" if mode == "ask" else ""
        lines.extend([f'  take boolean at "{prefix}/accepted" from verdict as accepted',
                      f'  take integer at "{prefix}/confidence_percent" from verdict as confidence'])
        # The helper compares its floored confidence to the explicitly supplied 80.
        # take enforces the integer contract; accepted controls all subsequent work.
        gate_lines = []
        if spec.get("log_labels"):
            lines.append('  take text at "/label" from verdict as label')
            gate_lines.append('if label is "error"')
        elif mode in {"classify", "ask"}:
            lines.append(f'  take text at "{prefix}/label" from verdict as label')
            gate_lines.append(f'if label is {compact(spec["label"])}')
            if mode == "ask":
                lines.extend(['  take boolean at "/answers/actionable/accepted" from verdict as action_accepted',
                              '  take integer at "/answers/actionable/confidence_percent" from verdict as action_confidence',
                              '  take boolean at "/answers/actionable/decision" from verdict as actionable'])
                gate_lines.extend(['if action_accepted is true', 'if actionable is true'])
        elif mode == "score":
            lines.extend(['  take json at "/score" from verdict as score_value',
                          '  run number.compare with left score_value and right 1 as score_order'])
            gate_lines.append('if score_order is ' + ('1' if spec["high"] else '-1'))
        else:
            lines.append('  take boolean at "/decision" from verdict as decision')
            gate_lines.append('if decision is true')
        lines.append('  if accepted is true')
        indent = 4
        for gate in gate_lines:
            lines.append(' ' * indent + gate)
            indent += 2
        success_lines = []
        result_value = candidate["result"]
        if mode in {"generate", "extract"}:
            instruction = spec["instruction"]
            kind = "generate" if mode == "generate" else "extract"
            second_expected = dict(provider="openai", kind=kind, model="gpt-6-luna", state=state, instruction=instruction)
            llm_call = f'llm.{"ask" if mode == "generate" else "extract"} with model "gpt-6-luna" and instruction {compact(instruction)}'
            if mode == "extract":
                success_lines.append('read schema_path as schema_text')
                llm_call += ' and schema schema_text'
                second_expected["schema"] = spec["schema"]
            success_lines.extend(['read source_path', 'if live is true', f'  then {llm_call}{suffix} as result_raw',
                                  'otherwise', f'  then {llm_call}{suffix} and fixture result_fixture as result_raw',
                                  'then parse json as generated'])
            if mode == "generate":
                success_lines.append('take text at "/text" from generated as content')
                result_value = spec["answer"]
            else:
                success_lines.append('take json at "/value" from generated as content')
                for name, field in spec["schema"]["properties"].items():
                    value_type = {"string": "text", "integer": "integer"}[field["type"]]
                    success_lines.append(f'take {value_type} at "/value/{name}" from generated as field_{name}')
                result_value = spec["value"]
            write(root / "result.json", dict(expected_request=second_expected, response=openai(spec.get("answer", compact(spec.get("value"))))))
            write(root / "refusal.json", dict(expected_request=second_expected, response=openai("", refused=True)))
            if mode == "extract":
                invalid = copy.deepcopy(spec["value"])
                first = next(iter(invalid))
                invalid[first] = 42  # Wrong type, not merely a different plausible value.
                write(root / "invalid.json", dict(expected_request=second_expected, response=openai(compact(invalid))))
            candidate["result"] = result_value
        result_start = len(success_lines)
        if spec.get("unix_only"):
            # Untrusted source is checked as literal text; it never becomes argv.
            success_lines.extend(['read source_path as requested', 'if requested is "pwd"',
                                  '  if confirmed is true', '    run "pwd" with arguments [] as diagnostic',
                                  '    ' + output(dict(candidate, status="executed-allowlisted-command")),
                                  '  otherwise', '    ' + output(dict(candidate, status="awaiting-confirmation")),
                                  'otherwise', '  ' + fallback_line])
            candidate["status"] = "awaiting-confirmation"
        elif spec.get("confirm"):
            success_lines.extend(['if confirmed is true', '  ' + output(dict(candidate, status="confirmed-draft"), "content"),
                                  'otherwise', '  ' + output(dict(candidate, status="awaiting-confirmation"), "content")])
            candidate["status"] = "awaiting-confirmation"
        else:
            success_lines.append(output(candidate, "content" if mode in {"generate", "extract"} else None))
        if mode == "generate":
            result_lines = success_lines[result_start:]
            success_lines[result_start:] = ['if content is ""', '  ' + fallback_line, 'otherwise',
                                           *('  ' + line for line in result_lines)]
        lines.extend(' ' * indent + line for line in success_lines)
        for _ in gate_lines:
            indent -= 2
            lines.extend([' ' * indent + 'otherwise', ' ' * (indent + 2) + fallback_line])
        lines.extend(['  otherwise', '    ' + fallback_line, 'catch failure', '  ' + fallback_line,
                      f'then save to "out/{ordinal}.json"'])
        filename = f'{ordinal}-{spec["slug"]}.program'
        (HERE / filename).write_text('\n'.join(lines) + '\n', encoding="utf-8", newline="\n")
        for name, resp in [("confident", response), ("low-confidence", low_response), ("confident-negative", other_response)]:
            write(root / f"{name}.json", dict(expected_request=expected, response=resp))
        # Invalid provider shape is a deterministic refusal path for Jev.
        write(root / "provider-error.json", dict(expected_request=expected, response={"model": "jev-latest", "answers": {}}))
        cases = []

        def case(name, inputs, expected_output):
            data = compact(expected_output) + '\n'
            cases.append(dict(name=name, inputs=inputs, expected_stdout=data, outputs={f'out/{ordinal}.json': data}))

        case("confident", {}, candidate)
        for name in ("low-confidence", "confident-negative", "provider-error"):
            inputs = dict(fixture=f"fixtures/{ordinal}/{name}.json")
            if mode in {"generate", "extract"}:
                inputs["result_fixture"] = f"fixtures/{ordinal}/DO-NOT-CALL.json"
            case(name, inputs, fallback)
        if mode in {"generate", "extract"}:
            case("refusal", dict(result_fixture=f"fixtures/{ordinal}/refusal.json"), fallback)
            if mode == "extract":
                case("schema-rejects-wrong-type", dict(result_fixture=f"fixtures/{ordinal}/invalid.json"), fallback)
            else:
                injected = "$(touch MODEL_TEXT_EXECUTED); `touch MODEL_TEXT_EXECUTED`; & echo not-a-command"
                write(root / "literal.json", dict(expected_request=second_expected, response=openai(injected)))
                case("shell-syntax-is-data", dict(result_fixture=f"fixtures/{ordinal}/literal.json"), dict(candidate, result=injected))
                write(root / "empty.json", dict(expected_request=second_expected, response=openai("")))
                case("empty-answer", dict(result_fixture=f"fixtures/{ordinal}/empty.json"), fallback)
        if mode == "classify":
            write(root / "invalid-label.json", dict(expected_request=expected, response=jev({"decision": choice("invented", ["invented", "other"])})))
            case("closed-label-set", dict(fixture=f"fixtures/{ordinal}/invalid-label.json"), fallback)
        if mode == "score":
            write(root / "boundary.json", dict(expected_request=expected, response=jev({"decision": score(1)})))
            case("score-at-review-boundary", dict(fixture=f"fixtures/{ordinal}/boundary.json"), fallback)
        if spec.get("unix_only"):
            for name, text, scored in [("destructive", "rm -rf ~/Documents", score(1.9)),
                                       ("outside-allowlist", "printf hello", score(0.1))]:
                (root / f"{name}.txt").write_text(text, encoding="utf-8", newline="\n")
                write(root / f"{name}.json", dict(expected_request=dict(expected, state=text), response=jev({"decision": scored})))
                case(name, dict(source_path=f"fixtures/{ordinal}/{name}.txt", fixture=f"fixtures/{ordinal}/{name}.json", confirmed="true"), fallback)
        if spec.get("confirm"):
            case("human-confirmed", dict(confirmed="true"), dict(candidate, status="executed-allowlisted-command" if spec.get("unix_only") else "confirmed-draft"))
        if mode == "ask":
            weak_second = jev(dict(route=choice(spec["label"], spec["questions"]["route"]["labels"]), actionable=noul(0.6)))
            write(root / "second-low.json", dict(expected_request=expected, response=weak_second))
            case("second-answer-low-confidence", dict(fixture=f"fixtures/{ordinal}/second-low.json"), fallback)
        advantage = {"classify": "Map paraphrases to a closed set of source-owned IDs",
                     "judge": "Check contextual meaning with a confidence-gated boolean",
                     "score": "Rank fuzzy urgency or danger with a numeric review threshold",
                     "ask": "Combine independent typed judgements in one bounded request",
                     "generate": "Explain free text after an evidence-sufficiency gate",
                     "extract": "Turn free text into a strict schema-validated record"}[mode]
        workflows.append(dict(id=index, file=filename, task=spec["task"], advantage=advantage,
                              bash_would_need=spec["bash"], unix_only=spec.get("unix_only", False), cases=cases))
    write(HERE / "catalog.json", dict(version=1, workflows=workflows))


if __name__ == "__main__":
    build()
