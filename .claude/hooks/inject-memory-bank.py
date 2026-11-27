#!/usr/bin/env python3
"""按代码路径触发的 memory-bank 规范注入。

会话读写某个源码文件时，扫描 memory-bank/ 下带 `paths:` frontmatter 的规范文档，
把 glob 命中的那几份注入到上下文里。四个平台共用这一个引擎：

- Claude Code：PostToolUse，事后注入，不打断操作
- Codex：PreToolUse（含 apply_patch），命中时先拒一次，模型读完规范重试即放行
- OpenCode：由 .opencode/plugins/inject-memory-bank.js 调用，同 PreToolUse 语义
- Antigravity：PostToolUse 匹配暂存 + PreInvocation 通过 injectSteps 注入，不打断操作

设计要点：
- 同一份规范在一个会话内只注入一次；规范内容改了会重新注入。
- 编辑规范文档自身时，清掉它的注入记录，让下次碰到相关代码立即生效。
- 「先拒一次」与去重机制自洽：重试时该规范已记录，不会再拒，因此不会死循环。
- 任何异常都静默退出（Antigravity 返回 `{}`），绝不打断用户的正常工作。

无外部依赖，frontmatter 为手写解析（只认 name / description / paths 三个字段）。
"""

from __future__ import annotations

from contextlib import contextmanager
import hashlib
import json
import os
import re
import sys
from pathlib import Path
from typing import Iterator

try:
    import fcntl
except ImportError:  # Windows 无 fcntl，降级为无锁原子替换
    fcntl = None  # type: ignore[assignment]

# 目录与容量约定
BANK_DIR = "memory-bank"
MAX_SPEC_CHARS = 9400  # 单份规范上限，超出则截断
MAX_TOTAL_CHARS = 9500  # 单次注入总量上限，超出的降级为索引行
STATE_DIR = "~/.cache/memory-bank-inject"
# apply_patch 补丁头，形如 `*** Update File: src/a.ts`
PATCH_PATH_RE = re.compile(r"^\*\*\* (?:(?:Add|Update|Delete) File|Move to): (.+)$")


def find_root(start: Path) -> Path | None:
    """从 start 向上找到包含 memory-bank/ 的项目根目录。"""
    for d in [start, *start.parents]:
        if (d / BANK_DIR).is_dir():
            return d
    return None


def glob_to_regex(pattern: str) -> re.Pattern[str]:
    """把 glob 转成正则。`**` 跨目录，`*` 与 `?` 不跨目录。

    先处理 `/**/` 这种「中间任意层级（也可以是零层）」的写法，
    避免 `src/**/*.ts` 漏掉 `src/a.ts`。
    """
    placeholder_any = "\x00"  # ** 占位
    placeholder_one = "\x01"  # * 占位
    p = pattern.replace("/**/", "/\x02/")  # 可选中间层级占位
    p = p.replace("**", placeholder_any)
    p = p.replace("*", placeholder_one)
    p = p.replace("?", "\x03")

    out = []
    for ch in p:
        if ch == placeholder_any:
            out.append(".*")
        elif ch == placeholder_one:
            out.append("[^/]*")
        elif ch == "\x02":
            out.append("\x02")  # 稍后整体替换
        elif ch == "\x03":
            out.append("[^/]")
        else:
            out.append(re.escape(ch))
    regex = "".join(out).replace("/\x02/", "/(?:.*/)?")
    return re.compile(f"^{regex}$")


def patch_paths(command: str) -> list[str]:
    """从 apply_patch 补丁文本的头部行里解析出涉及的文件路径。

    Codex 与 OpenCode 的 apply_patch 工具传的是整段补丁而非结构化路径，
    补丁头的形如 `*** Update File: src/a.ts`。
    """
    paths: list[str] = []
    for line in command.splitlines():
        m = PATCH_PATH_RE.fullmatch(line.strip())
        if m:
            p = m.group(1).strip()
            if p and p not in paths:
                paths.append(p)
    return paths


def parse_frontmatter(text: str) -> dict[str, object]:
    """解析文件头部的 YAML frontmatter，只取 name / description / paths。"""
    if not text.startswith("---"):
        return {}
    end = text.find("\n---", 3)
    if end == -1:
        return {}
    meta: dict[str, object] = {}
    paths: list[str] = []
    in_paths = False
    for line in text[3:end].splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if in_paths and stripped.startswith("- "):
            paths.append(stripped[2:].strip().strip("'\""))
            continue
        in_paths = False
        if ":" not in stripped:
            continue
        key, _, value = stripped.partition(":")
        key = key.strip()
        value = value.strip().strip("'\"")
        if key == "paths":
            in_paths = True
            if value:  # 支持 paths: [a, b] 的行内写法
                paths.extend(v.strip().strip("'\"") for v in value.strip("[]").split(",") if v.strip())
        elif key in ("name", "description"):
            meta[key] = value
    if paths:
        meta["paths"] = paths
    return meta


def load_specs(root: Path) -> list[dict[str, object]]:
    """加载 memory-bank/ 下所有带 paths 的规范文档（不含子目录）。"""
    specs = []
    for md in sorted((root / BANK_DIR).glob("*.md")):
        try:
            text = md.read_text(encoding="utf-8")
        except OSError:
            continue
        meta = parse_frontmatter(text)
        patterns = meta.get("paths")
        if not patterns:
            continue
        specs.append(
            {
                "name": meta.get("name") or md.stem,
                "description": meta.get("description", ""),
                "rel": md.relative_to(root).as_posix(),
                "body": text,
                "hash": hashlib.sha1(text.encode("utf-8")).hexdigest()[:12],
                "regexes": [glob_to_regex(str(p)) for p in patterns],
            }
        )
    return specs


def find_root_from_payload(data: dict[str, object]) -> Path | None:
    """按 cwd → workspacePaths → 进程 cwd 的顺序查找 memory-bank 根目录。"""
    candidates: list[Path] = []
    raw_cwd = data.get("cwd")
    if isinstance(raw_cwd, str) and raw_cwd.strip():
        candidates.append(Path(raw_cwd))
    ws_paths = data.get("workspacePaths")
    if isinstance(ws_paths, list):
        for wp in ws_paths:
            if isinstance(wp, str) and wp.strip():
                candidates.append(Path(wp))
    candidates.append(Path(os.getcwd()))

    for cand in candidates:
        root = find_root(cand)
        if root is not None:
            return root
    return None


def state_file(root: Path, session_id: str) -> Path:
    """会话级注入记录的存放位置（按项目路径分片，避免多项目串味）。"""
    project = hashlib.sha1(str(root).encode("utf-8")).hexdigest()[:16]
    safe_session = re.sub(r"[^A-Za-z0-9_-]", "_", session_id or "default")[:64]
    return Path(os.path.expanduser(STATE_DIR)) / project / f"{safe_session}.json"


def pending_file(root: Path, session_id: str) -> Path:
    """Antigravity 专用的待注入队列文件（PostToolUse 写入、PreInvocation 消费）。"""
    return state_file(root, session_id).with_suffix(".pending.json")


@contextmanager
def locked_session(state_path: Path) -> Iterator[None]:
    """对同一会话的状态读写加文件锁，避免并行工具调用产生写竞争。"""
    lock_path = state_path.with_suffix(".lock")
    fh = None
    try:
        lock_path.parent.mkdir(parents=True, exist_ok=True)
        fh = lock_path.open("a+", encoding="utf-8")
        if fcntl is not None:
            fcntl.flock(fh.fileno(), fcntl.LOCK_EX)
    except OSError:
        if fh is not None:
            try:
                fh.close()
            except OSError:
                pass
            fh = None
    try:
        yield
    finally:
        if fh is not None:
            try:
                if fcntl is not None:
                    fcntl.flock(fh.fileno(), fcntl.LOCK_UN)
            except OSError:
                pass
            try:
                fh.close()
            except OSError:
                pass


def atomic_write_json(path: Path, obj: object) -> None:
    """以临时文件 + replace 方式原子写入 JSON，避免并发读到半截内容。"""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f"{path.name}.{os.getpid()}.tmp")
    tmp.write_text(json.dumps(obj, ensure_ascii=False), encoding="utf-8")
    os.replace(tmp, path)


def read_json_dict(path: Path) -> dict[str, object]:
    """读取 JSON 对象文件，缺失或损坏时回退为空字典。"""
    try:
        val = json.loads(path.read_text(encoding="utf-8"))
        return val if isinstance(val, dict) else {}
    except (OSError, ValueError):
        return {}


def render_context(targets: list[str], specs: list[dict[str, object]]) -> str:
    """按字符预算组装 `<memory-bank-context>` 注入块。"""
    shown = "、".join(f"`{t}`" for t in targets[:3])
    if len(targets) > 3:
        shown += f" 等 {len(targets)} 个文件"
    header = f"<memory-bank-context>\n以下项目规范与你刚接触的 {shown} 相关，请在本次改动中遵守：\n\n"
    footer = "\n</memory-bank-context>"
    # 预算要扣掉外层包裹，否则累加出来的总量会超出 additionalContext 上限
    blocks, budget = [], MAX_TOTAL_CHARS - len(header) - len(footer)

    for spec in specs:
        body = str(spec["body"])
        if len(body) > MAX_SPEC_CHARS:
            body = body[:MAX_SPEC_CHARS] + "\n\n…（已截断，完整内容见该文件）"
        # 用标签包裹而非 markdown 标题，避免和规范正文里的 ### 小节混淆
        block = f'<spec path="{spec["rel"]}">\n{body}\n</spec>'
        if len(block) + 2 <= budget:  # +2 为块间分隔的空行
            blocks.append(block)
            budget -= len(block) + 2
        else:
            # 预算耗尽，降级为索引行，让模型知道有这份规范可以自己去读
            blocks.append(
                f'<spec path="{spec["rel"]}" truncated="true">\n'
                f"{spec['description']}（内容较长，需要时请直接读取该文件）\n</spec>"
            )

    return header + "\n\n".join(blocks) + footer


def extract_tool_info(data: dict[str, object]) -> tuple[str, dict[str, object]]:
    """兼容 Claude Code / Codex / OpenCode 与 Antigravity 的工具字段提取。"""
    tool_call = data.get("toolCall")
    if isinstance(tool_call, dict):
        name = str(tool_call.get("name") or "")
        args = tool_call.get("args") or {}
        return name, args if isinstance(args, dict) else {}
    name = str(data.get("tool_name") or data.get("toolName") or "")
    args = data.get("tool_input") or data.get("toolInput") or {}
    return name, args if isinstance(args, dict) else {}


def extract_raw_paths(tool_name: str, tool_input: dict[str, object]) -> list[str]:
    """提取工具调用涉及的原始文件路径列表。"""
    # Codex / OpenCode 的 apply_patch 传补丁全文，从补丁头解析涉及的所有路径
    if tool_name == "apply_patch":
        command = tool_input.get("command") or tool_input.get("patchText")
        if isinstance(command, str):
            return patch_paths(command)
        return []
    # Claude Code 用 file_path，OpenCode 用 filePath，Antigravity 用 Path / AbsolutePath / TargetFile
    for key in ("file_path", "filePath", "Path", "path", "TargetFile", "AbsolutePath"):
        fp = tool_input.get(key)
        if isinstance(fp, str) and fp.strip():
            return [fp.strip()]
    return []


def run(data: dict[str, object], is_antigravity: bool) -> None:
    """核心分发逻辑。Antigravity 任何路径都必须输出合法 JSON 对象。"""
    def emit_empty() -> None:
        if is_antigravity:
            print("{}")

    root = find_root_from_payload(data)
    if root is None:
        emit_empty()
        return

    session_id = str(data.get("conversationId") or data.get("session_id") or "")
    event_name = str(data.get("hookEventName") or data.get("hook_event_name") or "PostToolUse")
    state_path = state_file(root, session_id)
    pending_path = pending_file(root, session_id)

    # /clear 与 /compact（以及 Antigravity 新会话启动）时清空会话状态，
    # 避免旧的去重记录阻止规范再次注入。
    if event_name == "SessionStart":
        with locked_session(state_path):
            for p in (state_path, pending_path):
                try:
                    p.unlink(missing_ok=True)
                except OSError:
                    pass
        emit_empty()
        return

    # Antigravity PreInvocation / PostInvocation：消费 PostToolUse 暂存的规范并无打断注入。
    # 无待注入文件时直接返回 `{}`，不扫描 memory-bank 目录，保持毫秒级开销。
    if is_antigravity and event_name in ("PreInvocation", "PostInvocation"):
        if not pending_path.exists():
            print("{}")
            return
        with locked_session(state_path):
            if not pending_path.exists():
                print("{}")
                return
            pending = read_json_dict(pending_path)
            try:
                pending_path.unlink(missing_ok=True)
            except OSError:
                pass
        raw_targets = pending.get("targets")
        raw_specs = pending.get("specs")
        pending_targets = [str(t) for t in raw_targets] if isinstance(raw_targets, list) else []
        pending_specs = (
            [v for v in raw_specs.values() if isinstance(v, dict)]
            if isinstance(raw_specs, dict)
            else []
        )
        if not pending_specs:
            print("{}")
            return
        payload = render_context(pending_targets or ["当前文件"], pending_specs)
        print(
            json.dumps(
                {"injectSteps": [{"systemMessage": {"systemMessage": payload}}]},
                ensure_ascii=False,
            )
        )
        return

    tool_name, tool_input = extract_tool_info(data)
    if not tool_input:
        emit_empty()
        return

    raw_paths = extract_raw_paths(tool_name, tool_input)
    if not raw_paths:
        emit_empty()
        return

    targets: list[str] = []
    root_resolved = root.resolve()
    for rp in raw_paths:
        try:
            # 补丁头或相对路径是相对项目根的，不能按进程 cwd 解析——
            # hook 未必在项目根被拉起，那样会解析到别的目录去。
            p = Path(rp)
            t = (p if p.is_absolute() else root / p).resolve().relative_to(root_resolved).as_posix()
        except (ValueError, OSError):
            continue
        if t not in targets:
            targets.append(t)
    if not targets:
        emit_empty()
        return

    with locked_session(state_path):
        seen = read_json_dict(state_path)

        # 编辑规范文档自身：清掉它的记录（含待注入队列），让改动下次即刻重新注入
        if all(t.startswith(f"{BANK_DIR}/") for t in targets):
            changed = False
            stems = {Path(t).stem for t in targets}
            for stem in stems:
                changed = seen.pop(stem, None) is not None or changed
            if changed:
                try:
                    atomic_write_json(state_path, seen)
                except OSError:
                    pass
            if is_antigravity and pending_path.exists():
                pending = read_json_dict(pending_path)
                specs_map = pending.get("specs")
                if isinstance(specs_map, dict):
                    p_changed = False
                    for stem in stems:
                        p_changed = specs_map.pop(stem, None) is not None or p_changed
                    if p_changed:
                        try:
                            if specs_map:
                                atomic_write_json(pending_path, pending)
                            else:
                                pending_path.unlink(missing_ok=True)
                        except OSError:
                            pass
            emit_empty()
            return

        matched = [
            s for s in load_specs(root)
            if any(r.match(t) for r in s["regexes"] for t in targets)
            and seen.get(s["name"]) != s["hash"]
        ]
        if not matched:
            emit_empty()
            return

        for spec in matched:
            seen[str(spec["name"])] = spec["hash"]
        try:
            atomic_write_json(state_path, seen)
        except OSError:
            pass

        # Antigravity PostToolUse 不能直接返回上下文：暂存到会话队列，由下一次 PreInvocation 注入
        if is_antigravity:
            pending = read_json_dict(pending_path)
            p_targets = pending.get("targets") if isinstance(pending.get("targets"), list) else []
            p_specs = pending.get("specs") if isinstance(pending.get("specs"), dict) else {}
            for t in targets:
                if t not in p_targets:
                    p_targets.append(t)
            for spec in matched:
                p_specs[str(spec["name"])] = {
                    "name": spec["name"],
                    "rel": spec["rel"],
                    "description": spec["description"],
                    "body": spec["body"],
                }
            try:
                atomic_write_json(pending_path, {"targets": p_targets, "specs": p_specs})
            except OSError:
                pass
            print("{}")
            return

    payload = render_context(targets, matched)
    out: dict[str, object] = {
        "hookEventName": event_name,
        "additionalContext": payload,
    }
    # PreToolUse（Codex / OpenCode）没法给当前这轮补上下文，只能先拒掉这次调用，
    # 让模型读到规范后重试；重试时本份规范已记录过，不会再拒第二次。
    if event_name == "PreToolUse":
        out["permissionDecision"] = "deny"
        out["permissionDecisionReason"] = "已注入相关项目规范，请阅读后重试同一操作。"
    print(json.dumps({"hookSpecificOutput": out}, ensure_ascii=False))


def main() -> None:
    try:
        data = json.load(sys.stdin)
    except Exception:
        return
    if not isinstance(data, dict):
        return

    is_antigravity = (
        "hookEventName" in data
        or "conversationId" in data
        or isinstance(data.get("toolCall"), dict)
    )
    try:
        run(data, is_antigravity)
    except Exception:
        if is_antigravity:
            print("{}")


if __name__ == "__main__":
    try:
        main()
    except Exception:
        pass  # hook 永不打断主流程
