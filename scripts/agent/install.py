#!/usr/bin/env python3
"""Transactional, ownership-aware installer for Codex and pi integrations."""
from __future__ import annotations

import argparse
import copy
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib

from runtime import INSTRUCTIONS, TOOLS, VERSION, Runtime, repository

SOURCE = Path(__file__).resolve().parent


def reject_symlinks(path):
    """Check existing components before resolving or writing owned paths."""
    for component in (path, *path.parents):
        if component.is_symlink():
            raise ValueError(f"Refusing symlink in installation path: {component}")


def read(path):
    reject_symlinks(path)
    return path.read_bytes() if path.exists() else None


def atomic(path, content):
    reject_symlinks(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    if content is None:
        path.unlink(missing_ok=True)
        return
    mode = path.stat().st_mode & 0o777 if path.exists() else 0o600
    fd, temporary = tempfile.mkstemp(prefix=".tgrep-write-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as out:
            out.write(content)
            out.flush()
            os.fsync(out.fileno())
        os.chmod(temporary, mode)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def reject_inline_tables(path, text, key, dotted=False):
    """Refuse to append [key.child] headers after an inline/dotted definition.

    Keys after the first table header are not top level, so only the preamble
    can define the table these headers extend.
    """
    inline = re.compile(rf"\s*{re.escape(key)}\s*=")
    dotted_key = re.compile(rf"\s*{re.escape(key)}\s*\.")
    for line in text.splitlines():
        if re.match(r"\s*\[", line):
            break
        if inline.match(line) or (dotted and dotted_key.match(line)):
            raise ValueError(
                f"{path} defines {key} as an inline or dotted table; "
                f"convert it to [{key}.<name>] headers before installing")


def marked(agent, label, content, markdown=False):
    start = f"tgrep-agent:{agent}:{label}:begin"
    end = f"tgrep-agent:{agent}:{label}:end"
    if markdown:
        return f"\n<!-- {start} -->\n{content}\n<!-- {end} -->\n"
    return f"\n# {start}\n{content}\n# {end}\n"


class Changes:
    def __init__(self):
        self.before = {}
        self.after = {}

    def get(self, path):
        path = Path(path)
        if path not in self.before:
            self.before[path] = read(path)
        return self.after.get(path, self.before[path])

    def set(self, path, value):
        self.get(path)
        self.after[Path(path)] = value

    def remove_record(self, record):
        path = Path(record["path"])
        current = self.get(path)
        if current is None:
            return
        if record["kind"] == "file":
            if digest(current) != record["sha256"]:
                raise ValueError(f"Managed file was modified; preserving it: {path}")
            self.set(path, None)
        elif record["kind"] == "block":
            block = record["text"].encode()
            if current.count(block) != 1:
                raise ValueError(f"Managed block changed or disappeared; preserving {path}")
            value = current.replace(block, b"", 1)
            self.set(path, None if not value and record.get("created") else value)
        else:
            data = json.loads(current)
            groups = data.get("hooks", {}).get("SessionStart", [])
            if groups.count(record["entry"]) != 1:
                raise ValueError(f"Managed hook changed or disappeared; preserving {path}")
            groups.remove(record["entry"])
            if not groups and not record.get("had_event"):
                data["hooks"].pop("SessionStart", None)
            if not data.get("hooks") and not record.get("had_hooks"):
                data.pop("hooks", None)
            self.set(path, None if not data and record.get("created") else (json.dumps(data, indent=2) + "\n").encode())

    def file(self, path, value, records):
        if self.get(path) is not None:
            raise ValueError(f"Unmanaged file already exists: {path}")
        self.set(path, value)
        records.append({"kind": "file", "path": str(path), "sha256": digest(value)})

    def block(self, path, value, records):
        original = self.get(path)
        self.set(path, (original or b"") + value.encode())
        records.append({"kind": "block", "path": str(path), "text": value, "created": original is None})

    def commit(self, base):
        changed = {p: v for p, v in self.after.items() if v != self.before[p]}
        if not changed:
            return
        # Recheck before writes, retaining snapshots for manual recovery too.
        for path in changed:
            if read(path) != self.before[path]:
                raise ValueError(f"Configuration changed concurrently: {path}; retry")
        backup = base / "backups" / str(time.time_ns())
        reject_symlinks(backup)
        backup.mkdir(parents=True)
        metadata = {}
        for number, path in enumerate(changed):
            previous = self.before[path]
            metadata[str(path)] = str(number) if previous is not None else None
            if previous is not None:
                atomic(backup / str(number), previous)
        atomic(backup / "paths.json", (json.dumps(metadata, indent=2) + "\n").encode())
        done = []
        try:
            for path, content in changed.items():
                atomic(path, content)
                done.append(path)
        except BaseException:
            for path in reversed(done):
                atomic(path, self.before[path])
            raise


def agent_paths(agent, scope, root):
    if scope == "project":
        return root / (".codex" if agent == "codex" else ".pi")
    if agent == "codex":
        return Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))).expanduser().absolute()
    return Path(os.environ.get("PI_CODING_AGENT_DIR", str(Path.home() / ".pi/agent"))).expanduser().absolute()


def binary_path(args, base):
    if args.binary:
        binary = Path(args.binary).expanduser().resolve(strict=True)
    elif shutil.which("tgrep"):
        binary = Path(shutil.which("tgrep")).resolve()
    else:
        binary = base / "bin/tgrep"
        reject_symlinks(binary)
        if not binary.exists():
            checkout = SOURCE.parent.parent
            built = checkout / "target/release/tgrep"
            if not built.exists():
                if not shutil.which("cargo"):
                    raise ValueError("tgrep not found. Install it or provide --binary /path/to/tgrep (Cargo can build it from this checkout).")
                print("Building tgrep from this checkout with Cargo…", flush=True)
                subprocess.run(["cargo", "build", "--release", "--locked", "-p", "tgrep-cli"], cwd=checkout, check=True)
            binary.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(built, binary)
    subprocess.run([str(binary), "--version"], check=True, capture_output=True, timeout=5)
    help_text = subprocess.run([str(binary), "--help"], check=True, capture_output=True, text=True, timeout=5).stdout
    for flag in ("--no-index", "--index-path", "--json", "--files"):
        if flag not in help_text:
            raise ValueError(f"tgrep does not support required flag {flag}; upgrade it")
    return str(binary)


def install_agent(agent, args, root, base, binary, changes):
    records = []
    directory = base / agent
    runtime = directory / "runtime.py"
    configuration = directory / "config.json"
    # Resolve the user's cache root but refuse a symlinked tgrep-agent component.
    cache = Path(os.environ.get("XDG_CACHE_HOME", str(Path.home() / ".cache"))).expanduser().absolute() / "tgrep-agent"
    reject_symlinks(cache)
    flags = []
    if args.no_require_git:
        flags.append("--no-require-git")
    if args.no_max_filesize:
        flags.append("--no-max-filesize")
    elif args.max_filesize:
        flags += ["--max-filesize", args.max_filesize]
    config = {"root": str(root) if args.scope == "project" else None,
              "binary": binary, "cache_dir": str(cache), "index_flags": flags,
              "python": sys.executable}
    changes.file(runtime, (SOURCE / "runtime.py").read_bytes(), records)
    changes.file(configuration, (json.dumps(config, indent=2) + "\n").encode(), records)
    agent_dir = agent_paths(agent, args.scope, root)
    command = [sys.executable, str(runtime)]
    if agent == "codex":
        path = agent_dir / "config.toml"
        existing = (changes.get(path) or b"").decode()
        parsed = tomllib.loads(existing)
        servers = parsed.get("mcp_servers", {})
        if not isinstance(servers, dict):
            raise ValueError(f"{path} defines mcp_servers as {type(servers).__name__}; expected a table")
        if "tgrep" in servers:
            raise ValueError(f"An unmanaged MCP server named tgrep exists in {path}")
        if parsed.get("features", {}).get("hooks") is False:
            raise ValueError(f"Hooks are disabled in {path}; enable them before installing the full integration")
        reject_inline_tables(path, existing, "mcp_servers")
        if "hooks" in parsed:
            reject_inline_tables(path, existing, "hooks", dotted=True)
        block = "\n".join([
            "[mcp_servers.tgrep]", f"command = {json.dumps(sys.executable)}",
            "args = " + json.dumps([str(runtime), "mcp", "--config", str(configuration)]),
            "startup_timeout_sec = 10", "tool_timeout_sec = 40",
        ])
        hook = {"matcher": "startup|resume|clear|compact", "hooks": [{
            "type": "command", "command": shlex.join([*command, "ensure", "--config", str(configuration)]),
            "timeout": 5, "async": True,
        }]}
        hooks_path = agent_dir / "hooks.json"
        hooks_raw = changes.get(hooks_path)
        if "hooks" not in parsed:
            hooks = json.loads(hooks_raw or b"{}")
            had_hooks = "hooks" in hooks
            had_event = "SessionStart" in hooks.get("hooks", {})
            hooks.setdefault("hooks", {}).setdefault("SessionStart", []).append(hook)
            changes.set(hooks_path, (json.dumps(hooks, indent=2) + "\n").encode())
            records.append({"kind": "hook", "path": str(hooks_path), "entry": hook,
                            "created": hooks_raw is None, "had_hooks": had_hooks, "had_event": had_event})
        else:
            block += "\n\n[[hooks.SessionStart]]\nmatcher = " + json.dumps(hook["matcher"])
            block += "\n[[hooks.SessionStart.hooks]]\ntype = \"command\"\ncommand = " + json.dumps(hook["hooks"][0]["command"])
            block += "\ntimeout = 5\nasync = true"
        changes.block(path, marked(agent, "config", block), records)
        try:
            tomllib.loads(changes.get(path).decode())
        except tomllib.TOMLDecodeError as error:
            raise ValueError(
                f"Cannot merge the managed tgrep block into {path}: {error}. "
                "Convert inline or dotted mcp_servers/hooks tables to [table] headers, then retry") from error
        instruction_path = root / "AGENTS.md" if args.scope == "project" else agent_dir / "AGENTS.md"
        changes.block(instruction_path, marked(agent, "instructions", "Use the tgrep MCP tools when available. " + INSTRUCTIONS, True), records)
    else:
        substitutions = {"__PYTHON__": sys.executable, "__RUNTIME__": str(runtime), "__CONFIG__": str(configuration),
                         "__TOOLS__": TOOLS, "__INSTRUCTIONS__": INSTRUCTIONS}
        template = (SOURCE / "pi-extension.ts").read_text()
        template = re.sub("|".join(map(re.escape, substitutions)),
                          lambda match: json.dumps(substitutions[match.group()]), template)
        changes.file(agent_dir / "extensions/tgrep.ts", template.encode(), records)
    if args.scope == "project":
        changes.block(root / ".gitignore", marked(agent, "ignore", "/.tgrep-agent/"), records)
    return {"version": VERSION, "records": records, "config": config}


def prepare_records(manifest, agents, scope, root, base, action):
    """Rebase record locations, never their stored content/checksums, on a move.

    Validate an exact target allowlist before reading any manifest-owned path.
    Old locations may still exist (a copied checkout); never touch them.
    """
    for agent in agents:
        entry = manifest.get("agents", {}).get(agent)
        if not entry:
            continue
        old_root = entry["config"].get("root")
        moved = scope == "project" and old_root and Path(old_root) != root
        if moved and action == "doctor":
            raise ValueError("Project moved; run repair with --root pointing to the new project before using this integration")
        agent_dir = agent_paths(agent, scope, root)
        allowed = {base / agent / "runtime.py", base / agent / "config.json"}
        if agent == "codex":
            allowed.update({agent_dir / "config.toml", agent_dir / "hooks.json",
                            root / "AGENTS.md" if scope == "project" else agent_dir / "AGENTS.md"})
        else:
            allowed.add(agent_dir / "extensions/tgrep.ts")
        if scope == "project":
            allowed.add(root / ".gitignore")
        for record in entry["records"]:
            path = Path(record["path"])
            if moved:
                if not path.is_relative_to(old_root):
                    raise ValueError(f"Manifest path outside original project: {path}")
                path = root / path.relative_to(old_root)
            if path not in allowed or ".." in path.parts:
                raise ValueError(f"Unexpected manifest target: {path}")
            reject_symlinks(path)
            record["path"] = str(path)
        if moved:
            binary = Path(entry["config"]["binary"])
            if binary.is_relative_to(old_root):
                entry["config"]["binary"] = str(root / binary.relative_to(old_root))


def doctor(manifest, agents, root):
    failed = False
    for agent in agents:
        entry = manifest.get("agents", {}).get(agent)
        if not entry:
            print(f"[FAIL] {agent}: not installed in this scope")
            failed = True
            continue
        try:
            # Validation uses the same ownership checks as uninstall, without writes.
            check = Changes()
            for record in entry["records"]:
                if not Path(record["path"]).exists():
                    raise ValueError(f"Missing managed configuration: {record['path']}")
                check.remove_record(record)
            config_record = next(r for r in entry["records"] if r["path"].endswith("/config.json"))
            runtime_record = next(r for r in entry["records"] if r["path"].endswith("/runtime.py"))
            config = entry["config"]
            # Probe the interpreter recorded at installation time: the generated
            # Codex command and pi extension embed it, so a removed or replaced
            # Python must fail here rather than being reported as healthy.
            interpreter = config.get("python") or sys.executable
            subprocess.run([interpreter, "--version"], capture_output=True, check=True, timeout=5)
            subprocess.run([config["binary"], "--version"], capture_output=True, check=True, timeout=5)
            # Probe the installed MCP executable, rather than the checkout source.
            process = subprocess.Popen([interpreter, runtime_record["path"], "mcp", "--config", config_record["path"]],
                                       cwd=root, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                import selectors
                def exchange(message):
                    process.stdin.write(json.dumps(message) + "\n")
                    process.stdin.flush()
                    with selectors.DefaultSelector() as selector:
                        selector.register(process.stdout, selectors.EVENT_READ)
                        if not selector.select(40):
                            raise ValueError("MCP probe timed out")
                    reply = json.loads(process.stdout.readline())
                    if "error" in reply or reply.get("result", {}).get("isError"):
                        raise ValueError(str(reply))
                    return reply["result"]
                exchange({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "doctor", "version": VERSION}}})
                process.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
                process.stdin.flush()
                result = exchange({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
                if {t["name"] for t in result["tools"]} != {"search_code", "find_files"}:
                    raise ValueError("Unexpected MCP tools")
                exchange({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "find_files", "arguments": {"freshness": "current", "max_results": 1}}})
            finally:
                process.stdin.close()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                process.stdout.close()
                process.stderr.close()
            state = Runtime(config, root)
            print(f"[OK] {agent}: managed files, MCP initialization, tools and live filesystem query")
            print(f"[INFO] Service {'reachable' if state.status() is not None else 'not running (starts on hook or indexed query)'}; index: {state.index}")
            if not shutil.which(agent):
                print(f"[WARN] {agent} executable not on PATH; integration is staged")
        except (OSError, ValueError, subprocess.SubprocessError) as exc:
            print(f"[FAIL] {agent}: {exc}")
            failed = True
    if "codex" in agents:
        print("[INFO] Codex: restart the session and review/trust the hook in /hooks; project configuration must be trusted. Host policy may disable hooks.")
    return 1 if failed else 0


def main():
    parser = argparse.ArgumentParser(description="Install/uninstall tgrep MCP, startup hooks and guidance for Codex/pi (Linux/macOS; Python 3.11+).")
    parser.add_argument("action", nargs="?", choices=["install", "uninstall", "repair", "doctor"], default="install")
    parser.add_argument("--agent", help="codex, pi, or codex,pi")
    parser.add_argument("--scope", choices=["project", "user"], default=None)
    parser.add_argument("--root", type=Path, default=Path.cwd(), help="Project or current session directory")
    parser.add_argument("--binary", help="Existing tgrep executable; otherwise use PATH or build from checkout")
    parser.add_argument("--max-filesize")
    parser.add_argument("--no-max-filesize", action="store_true")
    parser.add_argument("--no-require-git", action="store_true")
    args = parser.parse_args()
    if args.max_filesize and args.no_max_filesize:
        parser.error("Choose --max-filesize or --no-max-filesize")
    if args.max_filesize:
        if not re.fullmatch(r"[0-9]+[KkMmGg]?", args.max_filesize):
            parser.error("--max-filesize must be a byte count or use K/M/G")
    if args.agent is None:
        if not sys.stdin.isatty():
            parser.error("Non-interactive use requires --agent codex, pi or codex,pi")
        args.agent = input("Agent [codex/pi/codex,pi] (codex): ").strip() or "codex"
        if args.scope is None:
            args.scope = input("Scope [project/user] (project): ").strip() or "project"
    args.scope = args.scope or "project"
    if args.scope not in ("project", "user"):
        parser.error("Invalid scope")
    agents = list(dict.fromkeys(a.strip() for a in args.agent.split(",")))
    if not agents or any(a not in ("codex", "pi") for a in agents):
        parser.error("--agent must be codex, pi or codex,pi")
    root = repository(args.root)
    data_home = Path(os.environ.get("XDG_DATA_HOME", str(Path.home() / ".local/share")))
    base = (root / ".tgrep-agent" if args.scope == "project" else data_home.resolve() / "tgrep-agent")
    reject_symlinks(base)
    manifest_path = base / "manifest.json"
    # Preflight all selected destinations before creating state, locks or binaries.
    for agent in agents:
        agent_dir = agent_paths(agent, args.scope, root)
        for path in (agent_dir / "config.toml", agent_dir / "hooks.json",
                     agent_dir / "extensions/tgrep.ts", base / agent / "runtime.py",
                     base / agent / "config.json"):
            reject_symlinks(path)
    owned = [manifest_path, base / "install.lock", base / "backups"]
    if args.scope == "project":
        # User scope never writes project-owned files, so unrelated symlinks in
        # the current repository must not block it.
        owned += [root / "AGENTS.md", root / ".gitignore"]
    for path in owned:
        reject_symlinks(path)
    if args.action in ("doctor", "uninstall") and not manifest_path.exists():
        print("No tgrep integration installed in this scope.")
        return 1 if args.action == "doctor" else 0
    base.mkdir(parents=True, exist_ok=True)
    with (base / "install.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        manifest = json.loads(manifest_path.read_text()) if manifest_path.exists() else {"format": 1, "agents": {}}
        if manifest.get("format") != 1:
            raise ValueError("Unsupported installation manifest format")
        prepare_records(manifest, agents, args.scope, root, base, args.action)
        if args.action == "doctor":
            return doctor(manifest, agents, root)
        changes = Changes()
        updated = copy.deepcopy(manifest)
        binary = None
        if args.action != "uninstall":
            previous_binaries = {manifest["agents"][a]["config"]["binary"] for a in agents if a in manifest["agents"]}
            if not args.binary and len(previous_binaries) == 1:
                previous_binary = previous_binaries.pop()
                if Path(previous_binary).is_file():
                    args.binary = previous_binary
            binary = binary_path(args, base)
        for agent in agents:
            old = updated["agents"].pop(agent, None)
            if old:
                for record in reversed(old["records"]):
                    changes.remove_record(record)
            if args.action != "uninstall":
                # Repair/reinstall preserves prior indexing options unless explicitly changed.
                agent_args = copy.copy(args)
                if old and not (args.max_filesize or args.no_max_filesize or args.no_require_git):
                    flags = old["config"].get("index_flags", [])
                    agent_args.no_require_git = "--no-require-git" in flags
                    agent_args.no_max_filesize = "--no-max-filesize" in flags
                    if "--max-filesize" in flags:
                        agent_args.max_filesize = flags[flags.index("--max-filesize") + 1]
                updated["agents"][agent] = install_agent(agent, agent_args, root, base, binary, changes)
        changes.set(manifest_path, (json.dumps(updated, indent=2) + "\n").encode())
        changes.commit(base)
    print(f"{args.action.capitalize()} complete: {', '.join(agents)} ({args.scope}).")
    if args.action == "uninstall":
        print(f"Removed only managed integrations. Indexes, shared services, binaries and recovery backups were retained in {base} and the configured cache.")
    else:
        print(f"tgrep: {binary}\nManifest: {manifest_path}")
        for agent in agents:
            if not shutil.which(agent):
                print(f"[WARN] {agent} not on PATH; configuration staged for when it is installed.")
        print("Restart your agent session. Codex users: review/trust the startup hook in /hooks. No index build is required before starting.")
        print(f"Verify: bash install-agent.sh doctor --agent {','.join(agents)} --scope {args.scope} --root {shlex.quote(str(root))}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.SubprocessError) as exc:
        print(f"tgrep-agent: {exc}", file=sys.stderr)
        sys.exit(1)
