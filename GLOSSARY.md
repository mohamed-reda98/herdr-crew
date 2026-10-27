# herdr-crew

Makes every agent that a main agent starts visible in Herdr, so you can watch what each one was asked and what it is doing.

## Agents

**Orchestrator**:
The agent you talk to, which starts other agents to help it.
_Avoid_: main agent, parent, coordinator

**Child**:
Any agent an orchestrator started: a delegate or a subagent.
_Avoid_: worker, helper

**Delegate**:
A child that runs as its own separate agent program (Codex or Claude), even when it is the same model as the orchestrator.
_Avoid_: implementer, relay

**Subagent**:
A child that runs inside the orchestrator's own process.
_Avoid_: sidechain, teammate

**Brief**:
The prompt the orchestrator gave a child when starting it.
_Avoid_: task, instructions, prompt

## Watching

**View**:
A read-only pane that shows one child: its brief at the top, then its steps live, then its result.
_Avoid_: viewer, watcher, mirror, monitor

**Step**:
One thing a child did that a view shows: a thought, a command, a file edit or a message.
_Avoid_: event, item
