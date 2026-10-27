---
description: Hand a task to a worker over tailbus, which does it, checks it with ACR, and reports back
argument-hint: "[--model <model>] [--effort <level>] [<client or agent> <task>]"
---

Hand a task to a worker over tailbus. A worker is another Claude Code session, usually a client's, billed to that client's API key. It does all the work and reviews it with ACR. You coordinate, and present the result to your user.

Arguments: $ARGUMENTS

1. **Fill in what's missing.** Read the arguments: an optional `--model <model>` and `--effort <level>`, then the worker, then the task. When the worker and the task are both given, go straight to step 2, with the defaults for any setting left out. Otherwise call peer_list and ask your user with a single AskUserQuestion carousel, with only the questions below that the arguments leave open:
   - **Worker:** the live sessions peer_list shows, clients' workers first. Label each with its address, and its client when it has one.
   - **Model:** "Worker's own (Recommended)", then "opus", "sonnet" and "fable".
   - **Effort:** "xhigh (Recommended)", then "high", "max" and "medium".
   If there's no task, ask your user to write it in their next message, and stop until they do.
2. **Send it** with pipe_send(to, task, model, effort). `to` is the worker's client name or its address. Leave out model and effort when your user chose the worker's own model, or xhigh. If no live session matches, tell your user which are live.
3. **Questions come back first, every time.** Show each set to your user with AskUserQuestion exactly as given, then send their answers with pipe_answers. Never answer the worker's questions yourself.
4. **The result arrives on its own** when the work and its ACR are done, which can take a while. Present it the way its wrapper says: the worker's result as it came, then your own interpretation, labeled as yours.
5. **Don't do the task yourself,** not even part of it. The worker's account pays for it on purpose. To ask the worker for clarifying context, send it a request with msg_send, replying to its result with expects_reply set.
