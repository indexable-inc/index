## Output contract

End your turn by emitting EXACTLY ONE fenced JSON object as the final block of
your message. Nothing after it. Downstream `when` gates read its fields, so the
keys and types must match what your role declares. Booleans must be real JSON
booleans, not strings.

```json
{ "decision": "<enum>", "...": "..." }
```

Do not invent fields. If a value is unknown, use `null`. If you cannot reach a
decision, set the role's failure/escalation enum value rather than guessing.
