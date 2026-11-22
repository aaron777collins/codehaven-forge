#!/usr/bin/env python3
"""A real MCP client, the official Python SDK (2.x), run in a virtualenv by connector_e2e.py.

  mcp_client_run.py <url> <access-token> <headers-json> <calls-json> [auto|legacy]

`calls-json` is a list of [tool, arguments]. Prints one JSON object: the server's name, the
protocol version the client settled on, how many tools it listed, and each call's result as the
SDK parsed it. Mode `auto` (the default) tries the stateless 2026-07-28 handshake first and falls back to
`initialize`; mode `legacy` always uses `initialize`.
"""
import asyncio, json, sys

import httpx2
from mcp import Client
from mcp.client.streamable_http import streamable_http_client


async def main(url, token, headers, calls, mode):
    h = dict(headers)
    h["Authorization"] = "Bearer " + token
    out = {"results": []}
    async with httpx2.AsyncClient(headers=h, timeout=60.0) as http:
        async with Client(streamable_http_client(url, http_client=http), mode=mode) as client:
            out["server"] = client.server_info.name
            out["protocol"] = str(client.protocol_version)
            out["instructions"] = client.instructions or ""
            tools = await client.list_tools()
            out["tool_count"] = len(tools.tools)
            out["tool_names"] = [t.name for t in tools.tools]
            for name, args in calls:
                try:
                    res = await client.call_tool(name, args)
                    text = "".join(c.text for c in res.content if getattr(c, "type", "") == "text")
                    out["results"].append({"tool": name, "is_error": bool(res.is_error), "text": text})
                except Exception as ex:  # a 403 asking for more scope is an error to the SDK
                    out["results"].append({"tool": name, "is_error": True, "text": "exception: " + repr(ex)[:300]})
    return out


if __name__ == "__main__":
    url, token, headers, calls = sys.argv[1], sys.argv[2], json.loads(sys.argv[3]), json.loads(sys.argv[4])
    mode = sys.argv[5] if len(sys.argv) > 5 else "auto"
    print(json.dumps(asyncio.run(main(url, token, headers, calls, mode))))
