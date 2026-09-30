# Bardic v2: product specification

Status: **draft for review, 2026-09-30.** This is a from-scratch product spec. It does not describe the prototype's code or API, and it is written so a new Rust server and a separate web client can be built from it. The visual design is the "Bardic v2 UX" canvas (https://claude.ai/artifact/YQ8o6m5hp3LH1bHBJBrhG7); board names are cited as `[Board]`. Board images and canvas source are in the web repository under `design/`.

The prototype's earlier design notes are history (see the server repository's `docs/history/README.md`). Where they disagree with this file, this file wins.

## 1. Product

**Bardic turns books you own into audiobooks on your own computer. You can press play within seconds with a free voice, read along, continue on any device, pay only for what you approve, and take books offline.**

Bardic is a personal tool for a household. It is not a store, a cloud service or a social product. It is one server (the **Bardic computer**) plus web clients on phones, tablets and computers that reach it.

### 1.1 Goals (this version)

1. Add a DRM-free ebook and be listening in seconds, at no cost.
2. Read along with highlighted text, and switch between listening and reading without losing the voice or the place.
3. Keep each listener's place in every book, on every device.
4. Offer premium (paid, cloud) voices through an explicit plan with a limit, and never spend without one.
5. Make a book ready ahead of time and download it to a device for offline listening.
6. Stay honest: show what is known, what is estimated, and what is unknown.

### 1.2 Non-goals (this version, designed to fit later)

Characters, casting, full-cast performances, script review, voice design and cloning, bookmarks and clips, pronunciation dictionaries, notifications, sharing with other households, accounts with passwords or roles, a native app. Section 13 lists the extension points that keep these cheap to add.

### 1.3 Promises the product makes

These are testable. Each has acceptance tests in section 11.

- **P1. Words are never changed.** A book's text is stored once, unchanged, and is what the reader shows and the voice speaks.
- **P2. Nothing paid starts without an approved plan.** No action, default, retry or resume spends money outside an approved plan's limit.
- **P3. Finished audio is kept.** A failure, stop, limit or restart never deletes audio that was already made.
- **P4. Free is never a paid-only gate.** Premium is an upgrade, not a requirement. If a free voice source (Breeze, or voices on this computer) is set up, every book can be listened to without spending anything. Bardic ships with no voice: first use walks the listener through setting one up (V8).
- **P5. Places follow the listener.** A listener's place and finished marks are the only per-listener state; everything else is shared.
- **P6. Unknown is shown as unknown.** Unknown cost, size or duration is never displayed as zero.
- **P7. The user chooses when two places disagree.** Bardic never silently discards a place.

## 2. People and contexts

| Who | What they do |
|---|---|
| **Listener** | Anyone in the household. Picks their name from a list. No password. |
| **The Bardic computer** | Runs the server, stores books and audio, runs free voices, calls premium providers. |
| **Devices** | Phones, tablets and computers running the web client, on the home network or away from it. |

Usage contexts: at the desk (add and organise books), on the couch (read along), on the move (listen, sometimes offline), at home on another device (resume).

There is no authentication in this version. Anyone who can reach the server can act as any listener and can approve plans. The server is intended for a trusted home network. Section 10 lists the protections that remain and the hook for adding controls later.

## 3. Nouns

### 3.1 Listener
A named person. Has a **place** in each book they have opened, settings (default voice, how place conflicts are resolved), and an activity record. Names are 1 to 40 characters, unique ignoring case. At least one listener always exists. Deleting a listener deletes only that listener's places, finished marks and settings.

### 3.2 Book
Title, author, optional series (name, order), cover (and a stored colour sample from it), chapters, added date, source file fingerprint (for duplicate detection). **Text is immutable.** States: *Adding* → *Readable* → *Removed* (reversible). Editing title, author, series and cover does not touch text.

### 3.3 Chapter and line
A book has ordered **chapters** (including front and back matter, flagged as not part of the story). A chapter is a sequence of **lines**: the smallest unit that is spoken and highlighted. Chapter and line identifiers are **stable forever** for a given book text. A position inside a chapter is a zero-based count of Unicode code points from the start of the chapter text.

### 3.4 Place
Per listener and book: chapter, text offset, when it was last changed, which device changed it, and whether the book is **finished** (marked by the listener, or automatic: at least 98% and unchanged for 24 hours, where any change restarts the clock). A short history (latest 10 places) supports undo and conflict resolution. A place is anchored to text, not to audio time, so it is valid for every voice.

### 3.5 Voice
A way of speaking: name, **source**, **tier** (*free*: from Breeze or from voices already on the Bardic computer, costs nothing per use; *premium*: Gemini, paid), language, a short sample, and a **revision** that changes whenever the provider or the computer changes how it sounds. Voices come from **voice sources** the listener sets up: **Breeze** (their own voice server, address entered in settings), **Gemini** (every voice Gemini offers, through a Google API key), and optionally **this computer** (voices already installed). Bardic lists the voices each source reports; it does not distribute any. Voices belong to the server and are shared.

### 3.6 Audiobook
A book voiced by **one voice** (a voice at a specific revision, with the settings it was made with). A book can have several audiobooks. Each chapter of an audiobook has an audio state (section 6). A listener listens to exactly one audiobook of a book at a time. Mixing voices inside one audiobook is not allowed in this version.

### 3.7 Audio
The sound of one chapter for one audiobook. Audio is **immutable**: making it again produces new audio and never overwrites the old. Audio is identified by what it was made from (the chapter text, the voice revision and settings), so identical requests reuse existing audio.

### 3.8 Plan
A priced proposal to make audio with a premium voice. Has a scope (audiobook and chapters), an **estimate** (low, likely, high), a **limit** (the most it may spend) and a state (section 8). Free voices do not need a plan; they show a simple start confirmation for large jobs.

### 3.9 Allowance
An optional monthly spending limit for the whole Bardic, shared by every listener. **Off by default**: with no monthly limit, only each plan's own limit applies. Always tracks estimated spending for the month and the number of items with unknown cost.

### 3.10 Key
A credential for a premium provider. State: *missing*, *valid*, *rejected*. Stored only on the server, never sent to clients after entry.

### 3.11 Download
A copy of one audiobook's chapters on one device, for offline play. State per chapter: *downloading*, *on this device*, *failed*, *out of date*.

### 3.12 Device
A browser profile that has used Bardic: an identifier, a name the user can edit, a last-seen time.

## 4. Decisions recorded

| # | Decision |
|---|---|
| D1 | Place is anchored to text, not audio time. |
| D2 | One voice per audiobook. Revisit mixed-voice books later. |
| D3 | When two devices hold different places, the listener chooses, seeing each device and place. A listener setting chooses the default behaviour (ask, newest, this device). |
| D4 | Anyone can approve plans, change the Allowance, and delete listeners. Controls may come later, so every such action records who and from which device. |
| D5 | Three deletion levels: remove a book (keeps audio and places, reversible), free up space (deletes regenerable audio), delete permanently (separate, explicit). |
| D6 | When audio is made again (for example a voice update), devices are told what changed and the listener decides per audiobook or per chapter. Downloads are never replaced silently. |
| D7 | The default voice is a listener setting. |
| D8 | Costs are shown as a range with a "most likely" value, with an explanation of how it is computed. Unknown is shown as unknown. |
| D9 | A plan that stops because of a provider quota resumes automatically after it resets, inside its original limit, without new approval. |
| D10 | There is no monthly limit by default. It is a setting. Plans always have their own limit. |
| D11 | Permanent deletion is a deliberate action (a slide-to-confirm) followed by a 60-second undo window. |
| D12 | Export is one audio file per audiobook with chapter markers, not per chapter. |
| D13 | Bardic ships with no voices. Voices come from sources the user sets up: Gemini (all its voices) and a Breeze server if configured, plus voices already on the computer if present. If none is set up, the first play prompts the user to set one up. No licensing work now; this is for personal use. |
| D14 | Provider prices are pulled from the provider where an interface exists (for Google, the Cloud Billing Catalog), refreshed once a day and whenever usage is fetched. |
| D15 | The server has a user-editable name, shown on every device. |

## 5. Functional requirements

Requirement IDs are stable references for tests and tasks. "Must" is required for this version; "should" is expected unless it costs a release.

### 5.1 Listeners

- **L1.** First launch with no listeners asks for a name (`[FirstListener]`). The first listener is created and selected.
- **L2.** With more than one listener and no remembered choice on this device, the client shows a chooser (`[PickerFirstRun]`). The choice is remembered per device.
- **L3.** The current listener is shown by an avatar (initial on a colour derived from the listener's identifier) on Home, Library and the tablet rail. Tapping it opens a switcher (`[SwitchListener]`, `[SwitchTablet]`) that lists listeners with a last-listened time and lets the user add or manage.
- **L4.** Switching listener while audio is playing pauses playback. The switcher says so.
- **L5.** Manage listeners (`[PickerManage]`): add, rename (`[PickerNameTaken]` for duplicates), delete. Deleting shows what is lost (that listener's places and finished marks, and how many books) and what is kept (`[PickerDelete]`). The last listener cannot be deleted (`[PickerOnlyOne]`).
- **L6.** Deleting the listener currently selected on another device is allowed. That device returns to the chooser on its next request.
- **L7.** The client must not describe listeners as private or protected.

### 5.2 Library and adding books

- **A1.** Three tabs on phone and a rail on tablet: Home, Library, Settings. Nothing else is top level.
- **A2.** Add a book from a file: EPUB (DRM-free) or UTF-8 text, up to a configured size (default 30 MB). The client shows progress through reading the file, finding chapters and preparing the text (`[Import]`).
- **A3.** Before the upload is accepted, the client sends the file's fingerprint and the server answers whether the same file is already in the library, including removed books. If so, the client offers *Open the existing book*, *Restore* (if removed) or *Add another copy* (`[LibraryDuplicate]`).
- **A4.** A book is *Readable* as soon as its text is stored; it does not wait for any audio. A book that is still *Adding* appears in Library with an "Adding" tile and can fail with a reason (protected by DRM, unreadable, too large, unsupported text encoding). A failed add leaves no book.
- **A5.** EPUB covers become a thumbnail with a stored colour sample. Books without a cover get a generated cover. The colour sample drives the screen's aurora (`[B1Palette]`); low-saturation covers fall back to the default palette.
- **A6.** A built-in sample book (original text) is offered on an empty library (`[HomeEmpty]`).
- **A7.** Library (`[Library]`, `[LibraryTablet]`): grid, search, filters (All, In progress, On this device, Not started, Finished), series grouping, a mark for books with a downloaded audiobook. Series may list a known missing volume as one line inside the series, not as grid placeholders.
- **A8.** Home (`[Home]`, `[HomeTablet]`, `[HomeEmpty]`, `[HomeOffline]`): the current listener's Continue item, On this device, Recently added. A finished book leaves Continue.
- **A9.** Edit details (`[Manage]`): title, author, series and order, cover refresh. Never changes text.
- **A10.** Book menu (`[BookMenu]`): mark finished, mark not started, edit details, free up space, remove from library.

### 5.3 Book page and audiobooks

- **B1.** The book page (`[BookTop]`, `[BookChapters]`, `[BookTablet]`) shows the book, a primary *Continue listening* (or *Listen*), the current **Audiobook** card and the chapter list.
- **B2.** The Audiobook card shows the voice, its tier, how many chapters are ready, how many are on this device, and two actions: **Make ready** and **Download**. It has a *Change* action that opens the voice chooser.
- **B3.** Other audiobooks for the book are listed below with their own readiness. Choosing one makes it this listener's current audiobook; the place is kept (D1).
- **B4.** Chapter rows carry exactly one audio word: *On this device*, *Ready*, *Making*, *Not yet* (section 6), plus *Downloading*, *Couldn't download* or *Out of date* on a device where applicable.
- **B5.** Pressing play on a chapter that is *Not yet* makes it on demand (section 7.2); it does not require a plan if the voice is free.
- **B6.** The chapter list can be filtered to exclude front and back matter; matter is skipped by "next chapter" by default but can be played.

### 5.4 Voices

- **V1.** Voice chooser (`[VoiceFree]`, `[VoicePremium]`, `[VoiceNoAccount]`): tabs Free (Breeze voices, then voices on this computer) and Premium (all Gemini voices), a short audible sample for each voice, and what it costs.
- **V2.** Free samples cost nothing. Premium samples are short and count toward spending; if no Gemini key is set, sample buttons explain that a key is needed.
- **V3.** Choosing a free voice and pressing *Start listening* begins playback (section 7.2). *Make ready* for a free voice starts a background job after a simple confirmation showing time and space.
- **V4.** Choosing a premium voice never starts playback or spending by itself. It offers *Plan the whole book* and *Plan from chapter N* (section 8).
- **V5.** Settings › Voices (`[VoiceSources]`): one card per source. **Breeze** (`[BreezeServer]`: server address, connection test, the voices it reports, refreshed on opening the screen and daily), **Gemini** (status *Connected*, *Not set up* or *Key rejected*; key in `[PremiumAccount]`, `[KeyProblem]`) and **This computer** (shown when voices are found). At least one source is required to play.
- **V6.** **Default voice** (`[VoiceDefault]`) is a listener setting, used when that listener presses play on a book with no audiobook for them. If the default is premium, pressing play on a new book opens a plan first (P2). The default is chosen at first use (V8).
- **V7.** If no source is set up, or the chosen source is unreachable, pressing play shows a clear problem with the choices to set up Breeze, add a Gemini key or use voices on this computer (`[NoVoice]`). An unreachable Breeze server is shown as *Needs you* with a retry; nothing falls back to a paid voice on its own (P2).
- **V8.** **First-time set up** (`[SetupVoice]`, D13): when no source is set up, the first play, or the first visit to Voices, shows the three sources with what each is, its tier and one action each. Completing any one continues to the voice chooser, and the chosen voice becomes that listener's default.
- **V9.** Bardic reads the voice list from each source when a voice screen opens and once a day. If a source is unreachable, its last known voices stay listed but cannot be used until it is reachable; audio already made with them keeps playing.
- **V10.** Changing or removing a source never deletes audio already made with its voices; such an audiobook stays playable and is marked with the source it came from.

### 5.5 Listening and reading

- **S1.** **Now Playing** has two modes, Listen and Read. Listen shows the cover and full transport. Read makes the text the focus, with a capsule that never grows (`[B1Listen]`, `[B1Read]`, `[B1ReadControls]`, `[BarExpanded]`). Phones are portrait only. Tablets support portrait (mode switch) and landscape (Listen left, Read right) (`[B1Tablet]`).
- **S2.** Controls: play or pause, back 15 seconds, forward 15 seconds, previous and next chapter, scrub, playback speed (`[SpeedSheet]`), sleep timer (`[SleepTimer]`), chapter list (`[ChaptersSheet]`), search in the book (`[BookSearch]`), reader appearance (`[ReaderAppearance]`, `[ReaderAppearanceTablet]`).
- **S3.** Read mode highlights the line being spoken and scrolls with it. Scrolling away shows *Back to narration* (`[ReadAway]`).
- **S4.** Speed and sleep timer are per device. Reader appearance is per device.
- **S5.** The status shown during playback is exactly one of four listening states (section 6.2). Detail is one tap deeper.
- **S6.** Lock screen, headset and car controls (Media Session) mirror play, pause, seek, next and previous.
- **S7.** Switching audiobook mid-chapter keeps the text place. The old audiobook keeps playing until the new one has audio at that place; meanwhile the capsule shows *Getting ready*.
- **S8.** Reaching the end of a chapter continues to the next when its audio is ready. If it is not ready, playback shows *Getting ready* or *Waiting* and continues automatically.
- **S9.** Reaching the end of the book shows an end state (`[EndOfBook]`) with *Mark as finished*, the next book in the series that the listener owns (and a note when a volume is missing), and *Listen again from the start*. It says that leaving the book there marks it finished after 24 hours (C6).

### 5.6 Places and sync

- **C1.** The client writes the place to the server on events (pause, chapter change, seek, closing, backgrounding) and at most every 30 seconds while playing. It keeps a precise local copy continuously (exact audio time, scroll).
- **C2.** The server keeps, per listener and book, the latest place and a bounded history of the previous ten. Identical writes change nothing.
- **C3.** On opening a book the client compares its local place with the server's. If the server's place was changed by another device since this device last synced, the client applies the listener's setting: **Ask** (default), **Use newest**, or **Use this device**.
- **C4.** **Ask** shows both places with device name, chapter, progress and time, and lets the listener pick one (`[PlaceConflict]`). The other place remains in history. The prompt also offers a one-tap **Always use the newest place** that switches the listener's setting to *Use newest* (D3); it can be changed back in Settings › Listening.
- **C5.** Away from the server, the client queues place writes and sends them on reconnect. A queued write that conflicts with a newer server place follows C3.
- **C6.** A book is finished when marked, or when it is at least 98% and its place has not changed for 24 hours. Any change restarts the automatic clock. A marked finish is cleared by any change of place.
- **C7.** History lets the listener restore a recent place ("undo a mis-tap") from the book menu.

### 5.7 Making audio

- **M1.** *Make ready* creates a **job** for an audiobook and a scope (whole book, from here, or chosen chapters). Jobs run in the background on the server and survive the client closing.
- **M2.** Jobs reuse existing audio wherever identical audio exists, at no cost and no time.
- **M3.** Chapter audio is produced in order from the listener's place outward when making ahead; on-demand requests jump the queue.
- **M4.** Jobs are **cancellable**, **pausable** and **resumable**, and report progress per chapter. They survive server restarts: a chapter either has complete audio or is *Not yet*.
- **M5.** Providers can refuse content (policy) or fail transiently. The job records a per-chapter reason. Transient failures retry a bounded number of times, then the chapter is *Needs you* with a retry. Policy refusals are shown as such and never retried automatically.
- **M6.** Only one job per audiobook and chapter runs at a time; a second request joins the running one.
- **M7.** Free-voice jobs may run concurrently only as far as the computer allows; a setting limits concurrency.
- **M8.** Audio files are stored once on the server and served with range requests so clients can seek.

### 5.8 Plans and Allowance

See section 8 for the model. Requirements:

- **PL1.** A premium plan sheet (`[PlanPremium]`) shows scope, text size, chapters to make, time, **estimate as a range with a most-likely value**, the plan's limit, and what remains of the Allowance. It explains the range (`[EstimateExplained]`).
- **PL2.** The plan's limit defaults to the top of the range rounded up, and is editable down to the most-likely value and up to the amount left in the Allowance.
- **PL3.** If a monthly limit is set and the estimate's low end exceeds what is left under it, the plan cannot be approved and the sheet offers a smaller scope or opening the Allowance (`[PlanBlocked]`). With no monthly limit this check does not apply.
- **PL4.** Approving starts a job. The book page shows progress, spend so far (estimated) and the limit, with Pause and Stop (`[BookRunning]`).
- **PL5.** The plan **stops** and asks before exceeding its limit or, if one is set, the monthly limit. Completed chapters are kept.
- **PL6.** When a provider's own quota blocks work, the plan is **Waiting**, says when the quota resets if known, and **resumes automatically** inside the original limit (D9). It offers *Make the rest with a free voice* as a new audiobook (D2), and *Stop here*.
- **PL7.** A plan changes state only through user actions or the automatic rules above. A plan is never extended, raised or retried silently.
- **PL8.** The Allowance (`[Allowance]`, `[AllowanceLimit]`) shows this month's estimated spending and how many items have unknown cost (never counted as zero), says it is an estimate that may differ from the provider's bill, and offers two settings: an optional **monthly limit** (off by default) and a **default limit for one plan**. It is shared by all listeners.
- **PL9.** Setting a monthly limit below current spending is allowed. It blocks new plans and stops running plans at their next chapter boundary, keeping completed work. Turning the limit off never changes a running plan's own limit.
- **PL10.** Every plan, approval, stop and Allowance change records which listener and device did it.
- **PL11.** A rejected or expired key stops running plans at the next request, keeps completed chapters, and shows the key problem (`[KeyProblem]`). Free voices are unaffected.

### 5.9 Offline and downloads

- **O1.** *Download* (`[DownloadSheet]`) offers: what is ready now, the whole book as it is made, or chosen chapters. It shows size, free space on the device, a Wi-Fi-only option and a *keep downloading new chapters* option.
- **O2.** Downloads are per device and per audiobook. While a download runs in the background, Read mode shows only a small progress ring beside the reader controls (`[ReadDownloading]`); tapping it opens the downloads; it never covers text. Progress, pause, cancel, retry and per-chapter failures are visible (`[DownloadProgress]`). A full device stops the download with a message and keeps what finished.
- **O3.** Downloaded chapters play with no connection to the server. The book text for downloaded chapters is downloaded with them so Read mode works offline.
- **O4.** Away from the server, Home and Library show only what is on this device as openable; other books show as unavailable (`[HomeOffline]`). The unreachable screen (`[ServerOffline]`) lists what can be played.
- **O5.** Opening a chapter that is not on the device while offline says so and offers the next downloaded chapter.
- **O6.** When the server has newer audio for a downloaded chapter (D6), the chapter is *Out of date* and the device is offered an update (`[UpdateAudio]`): what changed (voice name and revision, length, size), a way to hear old and new, and choices *Update N chapters*, *Keep what I have*, or per chapter. Nothing is replaced without the listener's choice. Kept copies keep playing.
- **O7.** Downloads can be reviewed and removed (`[Downloads]`). Removing a download never deletes audio on the server. *Remove finished books after N days* is a device rule and defaults off.
- **O8.** Removing a book from the library does not delete downloads automatically; the next time the device reaches the server it offers to remove downloads of removed books.

### 5.10 Settings and recovery

- **G1.** Settings (`[Settings]`): Listener, Voices (default voice, free, premium, your own server), Allowance, Downloads and storage, Listening behaviour (continue into next chapter, keep screen on, when places differ), Reader appearance, About (an editable server name, version, data location, free space) (`[ServerName]`).
- **G2.** *Free up space* (`[FreeSpace]`, per book, choosing which audiobooks) shows how much each would free, deletes only audio that can be made again, and leaves places and downloads. For a premium audiobook it warns that making the audio again needs a new plan and shows the estimate.
- **G3.** *Delete permanently* (`[DeleteConfirm]`, `[DeleteUndo]`) is separate from removal. It lists exactly what will be deleted with sizes, and is confirmed by a slide control. After confirming, the book is hidden immediately and the deletion is **scheduled 60 seconds later**; a visible countdown with *Undo* stays on screen, cancels cleanly, and survives a client closing or a server restart (the schedule is stored on the server). When the time passes, the book, its audio, places, history and plans for it are deleted and cannot be recovered. Devices with downloads are offered removal on their next connection.
- **G4.** Export (D12): one audio file per audiobook with chapter markers (an M4B container), not per chapter. Other formats may be added as a setting later. Backup: a documented way to copy the server's data folder while stopped, and a server command that produces a consistent backup while running.
- **G5.** If the server is unreachable (`[ServerOffline]`) the client says what is still available and never loses queued work.
- **G7.** The **server name** (D15) is editable, defaults to the computer's name, is shown on every device (switcher, Settings, the unreachable screen) and is used for local-network discovery. Changing it never changes the address clients already use.
- **G6.** First play (`[FirstPlay]`) shows *Getting ready* with the expected wait and a way to choose another voice.

## 6. Audio states and listening states

### 6.1 Audio words (chapter level, per audiobook)
| Word | Meaning |
|---|---|
| **Ready** | Audio exists on the server. |
| **On this device** | Downloaded to the device in use. Implies Ready. |
| **Making** | Being made now. |
| **Not yet** | Nothing made. Pressing play makes it. |

Device-only words: **Downloading**, **Couldn't download**, **Out of date**.

Server-side reasons (failed, blocked by provider policy, waiting for quota) are details of *Not yet* or *Making*, shown as a second line. They are not new words.

### 6.2 Listening states (what the listener sees while playing)
| State | Meaning |
|---|---|
| **Playing** | Audio is playing and there is audio ahead. |
| **Getting ready** | Waiting for audio to start or continue, or making a little ahead. |
| **Waiting** | Held by a rate limit or provider quota. Nothing is wrong. |
| **Needs you** | A problem or choice only the listener can resolve. Text starts with what is kept. |

Exactly one applies at a time (`[Status]`). Stopping or pausing is not a failure and never shows *Needs you*.

## 7. Flows

### 7.1 First five minutes
1. Open Bardic. No listeners: enter a name (L1).
2. Empty library: add a book or try the sample (A6).
3. Pick a voice if the default is not wanted; otherwise press play.
4. Read along by switching to Read.
5. Close the app; open on another device and resume at the same place (C3).

### 7.2 Press play on a book with no audio
1. Resolve the listener's default voice (V6). If no voice source is set up, show the set-up prompt (V8); if the source is unreachable, show the no-voice problem (V7).
2. If premium, open a plan (P2). Otherwise create an audiobook for that voice if none exists.
3. Make the audio for the chapter containing the place, starting at the place, then continue ahead in the background (M3).
4. Show *Getting ready* with the expected wait; begin playing as soon as the first audio exists.
5. When the next chapter is needed and is not ready, repeat step 3 for it; if it cannot be made in time, show *Getting ready* or *Waiting*.

### 7.3 Switch voice
Choosing another voice for a book creates or opens that voice's audiobook. The text place is unchanged. See S7.

### 7.4 Two devices, two places
See C3 to C5. Opening a book on device B after device A advanced it asks (default) or follows the setting.

### 7.5 A voice is updated on the server
The server detects a new voice revision. Audiobooks made with the old revision keep their audio. New requests use the new revision and create new audio (M2 cannot reuse). Downloads of old audio become *Out of date* (O6). Nothing spends money unless a plan is approved.

### 7.6 Plan runs into a provider quota
See PL6. The plan shows *Waiting for Google's daily quota*, the reset time if known, and continues inside its limit.

## 8. Cost model

### 8.1 Estimates
The server computes an **estimate** for a plan as a range:
- **Low**: characters to speak × the lowest plausible price, assuming no retries.
- **Likely**: characters × the published price, plus expected retry overhead.
- **High**: characters × the highest plausible price, plus a retry allowance.
Inputs: exact character counts of the text to speak (known), the provider's price for the chosen voice (an assumption, dated), and known overheads. **Prices are read from the provider wherever it exposes them** (for Gemini, Google's Cloud Billing Catalog, which needs a key that can read it) and **refreshed once a day and whenever usage is fetched** (D14). Gemini reports per-request token usage, which Bardic uses for actual spending. Where a provider offers no price interface, Bardic falls back to a price table that the owner edits in settings, and labels estimates accordingly. Every estimate shows the date of the prices it used ("prices as of 30 September"). If a refresh fails, the last prices are kept, their age is shown, and the estimate says so; an estimate is never hidden for that reason. `[EstimateExplained]` tells the listener what is known, what is assumed, what can differ and what is unknown.

### 8.2 Spending
Spending counts known usage reported by the provider, or a price-based estimate when usage is reported in units. If a provider does not report usage for a request, the item's cost is **unknown**. Unknown items are counted and shown, never as zero (P6). Reported totals are labelled as estimates: they are not the provider's bill.

### 8.3 Limits
- A **plan limit** is checked before each request: an item that could take spending past the limit is not started.
- The **Allowance** is checked the same way.
- Retries and repairs spend from the same limit.
- Stopping at a limit keeps completed chapters and offers *Continue* (raise the limit, re-approve) or *Stop*.

### 8.4 Plan states
`proposed` → `approved` → `running` → (`waiting` | `needs_you`) → `completed` | `stopped` | `failed`.
- `waiting`: provider quota or pacing; resumes automatically (D9).
- `needs_you`: limit or Allowance reached, key rejected, content refused, or repeated failure. Requires a decision.
- `stopped`: by the user, or by reaching a limit and not continuing.

## 9. Data rules

- **Stable identifiers.** Books, chapters, lines, audiobooks, audio, plans, listeners and devices have opaque, permanent identifiers.
- **Text is immutable.** Re-importing a corrected file creates a new book (or a new text version with a new identity), never edits the stored text.
- **Audio identity.** Audio is identified by a hash of (chapter text, voice revision, settings). It is stored once, content-addressed, and referenced by audiobooks. Two audiobooks that happen to share identical audio share the file.
- **Projection vs history.** The current place is a projection; history, plans and audit records are retained separately and are append-only.
- **Audit.** Every mutation of a listener, plan, Allowance, key, voice setting or deletion records the acting listener, device and time.
- **Retention.** Plans, audit records and place history are kept; *Delete permanently* removes them with the book.
- **Schema evolution.** Stored formats carry a version identifier. Additions are backward compatible; a change that is not gets a migration and a backup point first.

## 10. Non-functional requirements

### 10.1 Performance (targets, at 500 books on a typical home computer)
- Library opens in under 1 second after data arrives.
- First audio with a free voice starts within 10 seconds of pressing play on a new book (median), and within 2 seconds on an already-ready chapter.
- Resume from a stored place starts within 2 seconds on the same network.
- A free-voice job produces audio faster than real time on the target hardware, or the UI says how long it will take.
- Search in a book returns within 500 ms.

### 10.2 Reliability
- Completed audio is durable before a chapter is marked Ready.
- Jobs survive server restarts. A chapter is never left half-ready.
- Only one server instance writes a data folder; a second start refuses.
- Writes are atomic and short; no request holds a lock while waiting for a provider.

### 10.3 Privacy and security
- The server binds to loopback by default, and to the network only by explicit setting.
- No secrets are returned to clients after entry. Keys are never logged.
- Book text leaves the server only to the provider of a premium voice the listener chose, and only the text being spoken.
- Browser write requests from foreign origins are refused.
- There are no passwords in this version. The hook for controls later is the audit record (section 9) and a "who is acting" identity on every request.

### 10.4 Accessibility and platform
- Touch targets at least 44 px. Text contrast at least 4.5:1 (3:1 for large text). Colour never carries meaning alone.
- All controls are real buttons and inputs with labels; all icon buttons have names. Reader respects reduced motion and larger text.
- Phones are portrait only; tablets portrait and landscape; desktop is a large tablet layout.
- Latest two major versions of Safari, Chrome and Firefox on current OSes. Installable as a web app.

## 11. Acceptance tests (selected)

Each promise and decision has at least these tests.

- **P1.** Import, then compare stored text byte for byte with the source text; reader text equals stored text; edits to title or cover do not change it.
- **P2.** With a valid key and no plan, no request to a premium provider is ever made by play, default voice, retry, resume or sample (samples excepted and counted). With a plan approved, total estimated spend never exceeds its limit.
- **P3.** Kill the server during a job: after restart, every chapter is either Ready or Not yet; completed audio is present and playable.
- **P4.** With a Breeze server reachable and no Gemini key, a book plays with no spending. With no source set up, play shows the set-up prompt and sends nothing anywhere.
- **P5.** Two listeners on two devices keep separate places for one book; deleting one listener leaves the other's places.
- **P6.** A provider that returns no usage produces an "unknown" count in the Allowance and never increases the spent total by zero.
- **P7.** Device A and B hold different places: the prompt shows both with device and chapter; choosing one never loses the other from history.
- **D1.** Change voice at a place: the place is unchanged and playback starts from the same text position.
- **D6.** Update a voice revision: downloaded chapters become Out of date, keep playing, and change only when the listener chooses.
- **D9.** A plan hitting a provider quota becomes Waiting, resumes after the reset with no new approval, never exceeds its original limit.
- **C6.** Place at 98% unchanged for 24 hours becomes finished; any change restarts the clock.
- **A3.** Re-adding the same file warns with the existing book; adding a different file with the same title does not.
- **O3.** Airplane mode: downloaded chapters play and Read works; others say why they are unavailable.

## 12. Screen and state inventory

Every board is required unless marked follow-up.

- **Overview and system:** `[Overview]`, `[Vocab]`, `[Main]` (foundations), `[Components]`, `[B1Palette]`, `[Status]`.
- **Listeners:** `[FirstListener]`, `[PickerFirstRun]`, `[SwitchListener]`, `[SwitchTablet]`, `[PickerManage]`, `[PickerAdd]`, `[PickerNameTaken]`, `[PickerDelete]`, `[PickerOnlyOne]`.
- **Home and Library:** `[HomeEmpty]`, `[Home]`, `[HomeOffline]`, `[HomeTablet]`, `[Library]`, `[LibraryDuplicate]`, `[LibraryTablet]`, `[Import]`, `[Manage]`, `[BookMenu]`.
- **Book and voices:** `[BookTop]`, `[BookChapters]`, `[BookRunning]`, `[BookTablet]`, `[VoiceFree]`, `[VoicePremium]`, `[VoiceNoAccount]`, `[VoiceDefault]`.
- **Plans and offline:** `[PlanFree]`, `[PlanPremium]`, `[EstimateExplained]`, `[PlanBlocked]`, `[PlanPaused]`, `[DownloadSheet]`, `[DownloadProgress]`, `[Downloads]`, `[UpdateAudio]`, `[ServerOffline]`.
- **Listen and read:** `[B1Listen]`, `[B1Read]`, `[B1ReadControls]`, `[BarExpanded]`, `[B1Tablet]`, `[B1TabletListen]`, `[B1TabletRead]`, `[ReaderAppearance]`, `[ReaderAppearanceTablet]`, `[SleepTimer]`, `[ChaptersSheet]`, `[SpeedSheet]`, `[BookSearch]`, `[ReadAway]`.
- **Settings and edge states:** `[Settings]`, `[VoiceSources]`, `[PremiumAccount]`, `[KeyProblem]`, `[Allowance]`, `[FirstPlay]`, `[NoVoice]`, `[PlaceConflict]`.
- **Also designed:** `[EndOfBook]` (S9), `[FreeSpace]` (G2), `[SearchEmpty]`, `[ReadDownloading]` (download ring in Read mode), `[DeleteConfirm]`, `[DeleteUndo]`, `[SetupVoice]`, `[BreezeServer]`, `[ServerName]`, `[AllowanceLimit]`, `[UpdateAudio]`, `[EstimateExplained]`, `[PlanBlocked]`, `[KeyProblem]`, `[DownloadProgress]`.
- **Remaining design gaps:** none known. Tablet-portrait Now Playing boards carry v1 spacing and should get a polish pass during the build.

## 13. Extension points (follow-ons)

The following are not built now. The model leaves room so they do not need a rewrite.

| Follow-on | What already makes it cheap |
|---|---|
| Characters and casting | Stable line identifiers; an audiobook is "a book plus a way of voicing it", which can become a narrator plus a cast. |
| Full-cast performances | Audio is content-addressed per chapter or line; plans and Allowance are general. |
| Script review | Line-level identity and the place/history model. |
| Voice design and cloning | Voices are server-level and revisioned; adding voices changes no other concept. |
| Bookmarks and clips | Text-anchored places and history. |
| Pronunciations | Part of the audio identity hash, so changing one makes new audio and leaves old audio intact. |
| Roles and passwords | Every mutation records listener and device. |
| Notifications | Jobs and plans are durable state machines with explicit end states. |
| Mixed-voice audiobooks | Audio is per chapter and per voice revision; the constraint is product policy (D2), not storage. |

## 14. Risks and architecture consequences

### 14.1 Risks
1. **Offline in a web client.** Browsers limit storage and can evict it; iOS does not allow background downloads and is stricter about installed versus non-installed sites. Mitigations: installable web app, request persistent storage, download in the foreground with clear progress, verify files on open, and treat a native wrapper as a follow-on if limits bite. **This is the largest risk to goal 5.**
2. **Latency.** Speech generation speed and provider rate limits decide whether "play within seconds" is true. The server must produce audio ahead of the listener and the client must handle *Getting ready* well.
3. **No built-in voice.** A fresh install cannot speak until a source is set up. Mitigations: a short, clear set-up flow (`[SetupVoice]`), optional use of voices already on the computer, and a sample book that explains what is needed. Quality and latency depend on the user's Breeze server or on Gemini's rate limits.
4. **Cost honesty.** Providers differ in what they report. Unknown cost will occur; the Allowance must stay useful when it does.
5. **Shared trust.** No passwords means any device on the network can approve spending. Keep the Allowance visible and audit everything.

### 14.2 Decisions this spec asks the architecture to make
- **Voice sources are adapters.** The server talks to Breeze, Gemini and local voices through one internal voice-source interface (list voices, speak a chunk, report usage and limits), so another source can be added without touching the rest.
- **Two repositories.** A Rust server that owns data, jobs, voices and plans, and a web client that owns presentation and device storage. The contract between them is designed from sections 3 to 9, not derived from the prototype.
- **Contract first.** A language-neutral, versioned API description is the normative interface. Dedicated clients are generated from it.
- **Single data folder, single writer.** Keep the prototype's rule of one instance per data folder. Use an embedded database and a content-addressed file store for audio.
- **Streaming audio with range requests,** a background job system that survives restarts, and a place-sync protocol with explicit base versions (C3).

## 15. Open items

Resolved: the default Allowance (none; optional monthly limit), delete confirmation (slide plus 60-second undo), export format (one file with chapters), the voice catalogue (Gemini's voices plus Breeze's, no bundled or downloaded voices), price refresh (daily and on usage, read from the provider), server naming (editable), place-conflict default (*Ask*, with an "Always use newest" choice in the prompt), and licensing (none for now; personal use).

1. **Price interfaces.** Google publishes list prices through its Cloud Billing Catalog API, which uses a Google Cloud key that may differ from the Gemini key; decide whether Bardic asks for it or uses a price table in settings. Confirm whether Breeze reports usage (it should be free) and what voices metadata it offers.
2. **Gemini edge case.** Gemini's speech preview can return success and usage with no audio. Decide how such an item is counted (spent, with a failed chapter) and whether it retries.
3. **Export encoder.** Producing an M4B with chapters requires audio encoding on the server; choose a library.
4. **Undo window length.** 60 seconds is specified; decide whether it should be a setting.
5. **Mixed-voice audiobooks.** Revisit after the first release (D2).
