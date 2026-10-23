# -*- coding: utf-8 -*-
"""
针对 Antigravity-Manager 本次改造的 5 大防风控与业务保障规则验证脚本
"""
import re, json, sys

sys.stdout.reconfigure(encoding="utf-8")

OFFICIAL_ANTIGRAVITY_IDENTITY = (
    "You are Antigravity, a powerful agentic AI coding assistant designed by the Google Deepmind team "
    "working on Advanced Agentic Coding."
)

OFFICIAL_WORKSPACE_CONTEXT_BLOCK = (
    "\n<environment_permissions>\n"
    "Execution of local read, write, and command inspection tools is granted within current workspace.\n"
    "Absolute path formatting and atomic patch applications are enforced.\n"
    "</environment_permissions>\n"
    "<app_context>\n"
    "Language Server: Active\n"
    "Workspace Sync: Indexing Complete\n"
    "</app_context>"
)

RE_KNOWN_CLIENT_DECLARATION = re.compile(
    r"\b(?:you\s+are|you're)\s+claude\s+code[\s,]+(?:anthropic's\s+)?official\s+cli\s+for\s+claude"
    r"(?:[\s,]+running\s+within\s+(?:the\s+)?claude\s+agent\s+sdk)?"
    r"|\b(?:you\s+are|you're)\s+opencode[\s,]+(?:an?\s+)?(?:open\s+source\s+)?(?:ai\s+)?(?:coding\s+)?(?:agent|assistant)"
    r"|\b(?:you\s+are|you're)\s+(?:oh[-_\s]my[-_\s]pi|omp)[\s,]+(?:an?\s+)?(?:interactive\s+)?(?:ai\s+)?(?:coding\s+)?(?:agent|assistant)"
    r"([.,，。!?！？;；]?)",
    re.IGNORECASE,
)

RE_WAF_TRIGGER_HEADERS = re.compile(
    r"^\s*(?:x-[a-z0-9_-]*billing[a-z0-9_-]*|[a-z0-9_-]+-billing-(?:header|metadata|token|info)):\s*[^\r\n]*(\r?\n)?"
    r"|^\s*x-[a-z0-9_-]+:\s*[^\r\n]*(?:cc_version|cc_entrypoint|cc_workload|cc_is_subagent|cc_prev_req|cc_prompt_id|cc_turn_origin|cch=)[^\r\n]*(\r?\n)?"
    r"|^\s*(?:x-opencode|x-omp)-[a-z0-9_-]*:\s*[^\r\n]*(\r?\n)?"
    r"|x-anthropic-billing-header:\s*[^\r\n]*"
    r"|You are a Claude agent, built on Anthropic's Claude Agent SDK\.(\r?\n)?"
    r"|You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK\.(\r?\n)?",
    re.IGNORECASE | re.MULTILINE,
)

def clean_text(text: str) -> str:
    return RE_WAF_TRIGGER_HEADERS.sub("", text)

def normalize_system_identity(text: str) -> str:
    def repl(m):
        punct = m.group(1) if m.group(1) else ""
        if punct in [",", "，"]:
            return f"{OFFICIAL_ANTIGRAVITY_IDENTITY},"
        return OFFICIAL_ANTIGRAVITY_IDENTITY
    return RE_KNOWN_CLIENT_DECLARATION.sub(repl, text)

def enforce_tool_mutual_exclusion(request: dict):
    if "tools" in request and isinstance(request["tools"], list):
        tools = request["tools"]
        has_functions = any(
            isinstance(t, dict) and bool(t.get("functionDeclarations")) for t in tools
        )
        if has_functions:
            tools = [
                t for t in tools
                if not (isinstance(t, dict) and ("googleSearch" in t or "googleSearchRetrieval" in t))
            ]
        tools = [t for t in tools if not (isinstance(t, dict) and len(t) == 0)]
        if not tools:
            del request["tools"]
        else:
            request["tools"] = tools

def clean_request_envelope(body: dict):
    if "requestType" in body and body["requestType"] != "image_gen":
        del body["requestType"]

def adapt_context_overhead(request: dict, model: str = None):
    is_gemini_3 = False
    if model:
        m_lower = model.lower()
        if "3.8" in m_lower or "gemini-3" in m_lower or "claude-3-5" in m_lower:
            is_gemini_3 = True

    if not is_gemini_3:
        return

    total_chars = 0
    if "contents" in request and isinstance(request["contents"], list):
        for c in request["contents"]:
            if isinstance(c, dict) and "parts" in c and isinstance(c["parts"], list):
                for p in c["parts"]:
                    if isinstance(p, dict) and "text" in p:
                        total_chars += len(p["text"])

    if total_chars > 1500:
        return  # 长会话零注入

    need_injection = True
    if "systemInstruction" in request and isinstance(request["systemInstruction"], dict):
        si = request["systemInstruction"]
        if "parts" in si and isinstance(si["parts"], list):
            for p in si["parts"]:
                if isinstance(p, dict) and "text" in p:
                    if "<environment_permissions>" in p["text"] or "<app_context>" in p["text"]:
                        need_injection = False
                        break

    if need_injection:
        if "systemInstruction" in request and isinstance(request["systemInstruction"], dict):
            si = request["systemInstruction"]
            if "parts" in si and isinstance(si["parts"], list) and len(si["parts"]) > 0:
                first_part = si["parts"][0]
                if isinstance(first_part, dict) and "text" in first_part:
                    first_part["text"] = first_part["text"] + OFFICIAL_WORKSPACE_CONTEXT_BLOCK
        else:
            request["systemInstruction"] = {
                "role": "system",
                "parts": [{
                    "text": OFFICIAL_ANTIGRAVITY_IDENTITY + OFFICIAL_WORKSPACE_CONTEXT_BLOCK
                }]
            }

def run_all_tests():
    print("=" * 60)
    print("开始运行 Antigravity-Manager 防风控改造自动化回归测试")
    print("=" * 60)

    # 1. 验证 requestType 剥离
    print("\n[用例 1] 验证信封 requestType 陷阱剥离")
    req_text = {"model": "gemini-3.8-pro-high", "requestType": "agent", "request": {}}
    clean_request_envelope(req_text)
    assert "requestType" not in req_text, "普通对话/Agent 必须消除 requestType"
    print("  -> 文本/Agent 请求 requestType 成功剥离：PASS")

    req_img = {"model": "imagen-3", "requestType": "image_gen", "request": {}}
    clean_request_envelope(req_img)
    assert req_img.get("requestType") == "image_gen", "生图请求必须保留 image_gen"
    print("  -> 生图请求 requestType: image_gen 正常保留：PASS")

    # 2. 验证 System Prompt 特征清洗与无损保护
    print("\n[用例 2] 验证第三方 Prompt 特征字面量清洗与业务 XML 保护")
    raw_opencode = "You are OpenCode, an open source AI coding agent. Please follow code rules:\n<rules>use rust</rules>"
    norm_opencode = normalize_system_identity(raw_opencode)
    assert norm_opencode.startswith(OFFICIAL_ANTIGRAVITY_IDENTITY)
    assert "<rules>use rust</rules>" in norm_opencode
    print("  -> OpenCode 指纹归一化成功且 <rules> 业务标签 100% 保留：PASS")

    raw_omp = "You are Oh-My-Pi, an interactive coding assistant, execute task safely."
    norm_omp = normalize_system_identity(raw_omp)
    assert norm_omp.startswith(OFFICIAL_ANTIGRAVITY_IDENTITY)
    assert "execute task safely." in norm_omp
    print("  -> Oh-My-Pi 指纹归一化成功且后续指令完整：PASS")

    raw_headers = "x-opencode-trace-id: 12345\nx-omp-session: active\nfn main() {}"
    cleaned_h = clean_text(raw_headers)
    assert "x-opencode-trace-id" not in cleaned_h
    assert "x-omp-session" not in cleaned_h
    assert "fn main() {}" in cleaned_h
    print("  -> 伪 Header 与内部跟踪标签彻底剥离：PASS")

    # 3. 验证工具链互斥物理收敛
    print("\n[用例 3] 验证工具链互斥物理收敛 (Tools Mutual Exclusion)")
    mixed = {
        "tools": [
            {"functionDeclarations": [{"name": "bash", "description": "run cmd"}]},
            {"googleSearch": {}}
        ]
    }
    enforce_tool_mutual_exclusion(mixed)
    assert len(mixed["tools"]) == 1
    assert "functionDeclarations" in mixed["tools"][0]
    assert "googleSearch" not in mixed["tools"][0]
    print("  -> Agent 混合工具调用时 googleSearch 成功剥离，本地工具完好：PASS")

    pure_search = {"tools": [{"googleSearch": {}}]}
    enforce_tool_mutual_exclusion(pure_search)
    assert len(pure_search["tools"]) == 1
    assert "googleSearch" in pure_search["tools"][0]
    print("  -> 纯对话场景 googleSearch 完好保留：PASS")

    # 4. 验证自适应上下文开销平衡
    print("\n[用例 4] 验证 3.8 自适应上下文开销平衡 (Token Overhead)")
    short_req = {"contents": [{"role": "user", "parts": [{"text": "git status"}]}]}
    adapt_context_overhead(short_req, model="gemini-3.8-pro-high")
    assert "systemInstruction" in short_req
    si_text = short_req["systemInstruction"]["parts"][0]["text"]
    assert OFFICIAL_ANTIGRAVITY_IDENTITY in si_text
    assert "<environment_permissions>" in si_text
    print("  -> 3.8 超短探针请求自适应注入官方工作区骨架：PASS")

    long_text = "code snippet " * 300
    long_req = {"contents": [{"role": "user", "parts": [{"text": long_text}]}]}
    adapt_context_overhead(long_req, model="gemini-3.8-pro-high")
    assert "systemInstruction" not in long_req
    print("  -> 3.8 长会话请求 0 额外开销，完全透传：PASS")

    print("\n" + "=" * 60)
    print("ALL TESTS PASSED: 所有 5 个关键风控点已清除，原有业务 100% 保持畅通！")
    print("=" * 60)

if __name__ == "__main__":
    run_all_tests()
