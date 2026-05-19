# Taste (Continuously Learned by [CommandCode][cmd])

[cmd]: https://commandcode.ai/

# Communication Style
- When user says "do it end to end", they want complete comprehensive implementation, not partial fixes. Confidence: 0.95
- When user says "dont mention yourself" (or variants), they explicitly do not want Claude attribution in commits. Confidence: 0.95
- User frequently uses "full prod grade" or "production grade" to indicate high quality expectations. Confidence: 0.85
- User frequently uses "high perf" or "extreme perf" to indicate performance is critical. Confidence: 0.85

# Tool Preferences
- User prefers recon MCP tools (`code_*`) over generic Read/Grep/Glob. When they say "use recon", use the MCP tools. Confidence: 0.90

# Workflow Patterns
See [workflow-patterns/taste.md](workflow-patterns/taste.md)
