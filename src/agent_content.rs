pub const INDEX_MD: &str = r"# OP_RETURN Bot

Write messages to the Bitcoin blockchain through OP_RETURN outputs. You pay the
fees with a Lightning or unified on-chain payment request.

- MCP server: `POST /mcp`
- MCP discovery: `/.well-known/mcp.json`
- REST API: `/api/create`, `/api/unified`, `/api/status/{rHash}`, and
  `/api/view/{txId}`
- API catalog: `/.well-known/api-catalog`
- Agent Skills: `/.well-known/agent-skills/index.json`
- Authentication: none; see `/auth.md`
";

pub const AUTH_MD: &str = r"# OP_RETURN Bot auth.md

OP_RETURN Bot has no authentication. It has no API keys, OAuth flow, bearer
tokens, user registration, or scopes. All REST and MCP endpoints are public.
Each write request returns a Lightning or unified Bitcoin payment request.
Payment authorizes that one write.

REST API documentation:
https://github.com/benthecarman/OP-RETURN-Bot/blob/master/docs/API.md
";

pub const SKILL_MD: &str = r"---
name: op-return-bot
description: Write messages to Bitcoin OP_RETURN outputs with a paid REST API or MCP server.
license: MIT
---

# OP_RETURN Bot

Use the streamable HTTP MCP server at `https://opreturnbot.com/mcp`, or use the
REST API. No authentication is required.

1. Call `POST /api/create` or `POST /api/unified` with `message` and optional
   `noTwitter` fields.
2. Pay the returned request.
3. Poll `GET /api/status/{rHash}` until it returns a Bitcoin transaction ID.
4. Read the message with `GET /api/view/{txId}`.

The maximum message size is 99,000 bytes.
";
