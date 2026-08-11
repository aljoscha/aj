# Banked UX work

Items found during a round but deliberately not fixed in it, batched so
polish does not starve structural work. Each carries the ruling that
settles it, so picking the batch up needs no new decision.

## The store's tag sentence names its subject twice

`Tag not set: a tag is at most 80 bytes, ...` reads "tag" twice. The
composer names the subject once and correctly, the repetition comes from
the store's own sentence.

Ruling: reword at the source in `aj-session`, not in the composer. The
same principle governs `HostError::Locked`: a sentence is produced where
the facts live and carried opaquely to every surface, so a caller must
never rewrite it. The sentence is shared by the TUI, the wire, and the
launch flag, so the fix lands once for all three.

## Two toasts name one failed branch

A refused head switch raises the refusal and `Branch failed. Your
message was restored to the editor.` stacks under it, naming the failed
action twice across the pair.

Ruling: collapse the two into one sentence. Do not suppress the second,
the message's restoration is information the user needs.
