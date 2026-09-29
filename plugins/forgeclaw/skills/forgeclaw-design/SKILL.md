---
name: forgeclaw-design
description: Plan a repository change with ForgeClaw before implementation, or continue review of a design proposal PR. Use when someone asks to design, plan, or refine a change rather than implement it yet.
---

# Design with ForgeClaw

Follow the `forgeclaw` skill for forge tools, subject scope, review replies, and branch permissions. Use this skill when the requested work is a design or plan. Ordinary implementation requests can proceed without a design PR.

Investigate the repository and the request enough to make a concrete recommendation. Identify the goal, relevant constraints, viable alternatives and tradeoffs, the proposed approach, implementation steps, and how the result would be verified. Ask about a decision only when the available context cannot resolve it; otherwise make a reasoned choice and keep moving.

Record a substantial proposal in a Markdown document in the same repository, using its existing design-document convention or `docs/designs/<topic>.md`. Open a PR containing the proposal so reviewers and agents can discuss specific lines and see revisions. Keep that PR focused on the design document; do not implement the proposed code in it. Link the originating issue or request when one exists.

Finish and commit the proposal before the first branch push. After opening the PR, check its remote diff contains the document before reporting it ready for review.

On later review turns, read the current proposal and comments, answer questions inline, and revise the document for requested design changes. Explain material decisions in the PR discussion. Keep working through review feedback within the design scope. Merging the design PR records the accepted plan; implementation belongs in a separate code PR when requested.
