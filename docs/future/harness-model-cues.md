---
status: current
summary: Quiet completion sounds and readable model capability and effort in Harness.
---

# Harness model cues

Harness stays quiet during navigation and routine tool steps. Sound marks a completed or failed
turn; music and rain require explicit opt-in. The completion cue is a short, soft pair of notes.

The model menu keeps provider groups and the older-version fold, sorting visible models by broad
capability tier before provider order. One UI policy classifies known model families as Frontier,
Balanced or Light; these are curated categories, not benchmark scores. Unknown families keep their
provider order and receive no guessed tier. Versions remain numeric, with older versions folded.

Chat sidebar rows show the model family name and a small thinking effort meter. Their tooltip
names the model and spells out its effort; missing information stays absent. The picker and sidebar
read the same policy and the same transcript/daemon choice, with no protocol or persisted-format change.

The model name and effort meter are separate inline controls. A click selects that chat and opens
the corresponding existing menu beside its row; model shortcuts reveal the sidebar. The bottom
status area keeps context and plan information. External transcripts show their model as read-only.
The hostname has a computer icon for the local device, with its local connection stated in a tooltip;
the icon is based on device identity rather than guesses from the hostname.

Cost: tier sorting adds O(n log n) to the menu's existing O(n²) older-version detection for n models; a sidebar row reads its model list
once and adds the existing text and effort-meter primitives. No timers, animation or background work are added.
Check the scene loader, picker tests, an offscreen sidebar and the rebuilt app. A numeric leaderboard
was rejected because no comparable benchmark measurements are available in the model catalogue.
