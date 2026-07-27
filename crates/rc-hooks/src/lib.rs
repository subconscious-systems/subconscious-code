//! rc-hooks: hook dispatch (§11.3).
//!
//! Not yet implemented — lands in M8. JSON on stdin, exit code + JSON on
//! stdout controls flow. Events: SessionStart, SessionEnd, UserPromptSubmit,
//! PreToolUse, PostToolUse, Stop, SubagentStop, PreCompact, Notification.
//! A broken hook logs and proceeds — it must never brick the agent.
