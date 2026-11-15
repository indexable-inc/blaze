---
status: current
summary: Quiet completion sounds and readable model capability and effort in Harness.
---

# Harness model cues

Harness stays quiet during navigation and routine tool steps. Sound marks a completed or failed
turn and explicit transitions into a negative agent mood; music and rain require explicit opt-in.
The completion cue is a short, soft pair of notes. Repeating the same negative mood stays silent.

The model menu keeps provider groups and the older-version fold, sorting visible models by broad
capability tier before provider order. One UI policy classifies known model families as Frontier,
Advanced, Balanced or Light; these are curated categories, not benchmark scores. Unknown families keep their
provider order and receive no guessed tier. Versions remain numeric, with older versions folded.

Chat sidebar rows keep the task title, mood, model family name, thinking effort and context ring on
one line. The compact ring moves percentage and exact counts into its tooltip and breakdown. Their tooltip
names the model and spells out its effort; missing information stays absent. The picker and sidebar
read the same policy and the same transcript/daemon choice. The daemon's session list carries an optional
compact context summary, so every row can show usage without subscribing to every transcript. Older
daemons omit the summary; absent usage stays unknown. Existing chat logs keep their format.

The model name and effort meter are separate inline controls. A click selects that chat and opens
the corresponding existing menu beside its row; model shortcuts reveal the sidebar. The context
ring also lives on its agent's row and opens that agent's breakdown. The bottom status area keeps
plan information. External transcripts show their model as read-only.
The hostname has a computer icon for the local device, with its local connection stated in a tooltip;
the icon is based on device identity rather than guesses from the hostname.

Provider-specific vector glyphs appear beside model names in both the sidebar and model menu:
Claude's radial glyph is orange; GPT's circuit glyph is mint. Curated capability adds spokes or
circuit layers in the same fixed footprint, while separate neutral bars still show thinking effort.
Unknown models retain a provider mark without a guessed strength. The glyphs reuse the sorting
policy, add no asset IO, and cost one small vector per visible model. Device headers fold their own
agents and running items without changing process state; device groups start expanded.

A saved chat whose provider process was lost is ready to resume, with no failure label or mark;
real nonzero exits and lost terminal/run processes keep their failure state. The UI distinguishes
conversation availability from provider process lifetime without changing the process protocol.

Command-O search and the composer bind Draft selection/IME state through one TextEdit/caret path
and respect the same Ghostty blink preference. The shared renderer reports whether a visible caret
is blinking; that keeps the calm frame clock running for every field, including overlays, without
a feature-specific list of owners. Hidden and steady carets do not request animation.

Agents report a live mood through `await updateEmotion(emotion, reason?)`. Each row shows its emoji
and a reason on hover. The native app owns this ephemeral state, scoped to a session and device;
it never persists or replays moods. A distinct uncertain, frustrated or blocked report emits a soft cue
unless `emotion_sound` or master `sound` is disabled. The always-on agent note asks for meaningful mood
updates and concrete understanding of the root objective, constraints, priorities and success criteria.

Cost: tier sorting adds O(n log n) to the menu's existing O(n²) older-version detection for n models; a sidebar row reads its model list
once and adds the existing text and effort-meter primitives. No timers, animation or background work are added.
Check the scene loader, picker tests, an offscreen sidebar and the rebuilt app. A numeric leaderboard
was rejected because no comparable benchmark measurements are available in the model catalogue.
