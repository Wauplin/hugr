# Android agent marketplace and host

This document describes an Android application that presents one conversational interface and lets users extend it with independently packaged Hugr agents, then evaluates the equivalent iPhone and iPad product. It covers the product model, runtime architecture, marketplace, platform integration, store acceptance, permissions, background execution, voice, security, data, operations, existing products, and the limits the product should enforce. It is a design, not an implementation sequence.

## Product model

The application is a host for a built-in orchestrator and a user-selected set of specialist agents. The orchestrator handles the conversation, decides when a specialist is useful, delegates a bounded question, and turns the specialist's structured answer into a response for the user. A weather agent, note taker, trip planner, Slack agent, and GitHub coding agent all appear to the orchestrator through the same ask-and-answer contract even though they use different tools and services.

An installed agent is called a plugin in the user interface. A plugin is not an Android plugin and must not be arbitrary downloaded native or JVM code. It is a signed, declarative agent package containing prompts, model and tool requirements, schemas, limits, metadata, and optional portable logic accepted by the host. Android platform effects remain implementations owned by the app. This separation is the foundation for marketplace safety, Google Play compatibility, and meaningful permission enforcement.

A preset is an immutable-at-use snapshot of a configuration: an orchestrator profile, enabled plugin versions, per-plugin settings and grants, model choices, budgets, routing preferences, voice settings, and optional starting instructions. Users can install published presets, clone and edit them, or build their own. Starting a chat pins the chosen preset revision so later edits or plugin upgrades cannot silently change a running conversation.

A chat is a durable task container, not an Activity or process lifetime. It owns a message timeline, orchestration traces, delegated child traces, attachments, pending approvals, budget use, and an execution state. Several chats may be active independently. The operating system may stop the process at any time; persisted state must be sufficient to recover without duplicating completed external actions.

## Hugr in plain language

Hugr separates an agent's deterministic decision loop from the outside world. Its core consumes events such as user input, model output, tool results, permission decisions, and time ticks, then emits commands such as starting a model call or invoking a capability. The core performs no network, file, clock, process, or Android API access. A host implements those effects and feeds their results back to the core.

Every specialist uses a common `Ask` input and `Answer` output. The answer includes a structured response plus mandatory status, cost, duration, token, and trace metadata. A returned trace identifier can resume that specialist later. Traces are immutable and replayable, and a resumed run creates a new trace linked to its parent. The Android app should preserve these properties using Android storage implementations rather than the desktop filesystem backends.

Hugr currently ships standalone native agent binaries and a WebAssembly brain used by browser and TypeScript hosts. Neither artifact should be treated as an Android marketplace format without a new mobile packaging boundary. Desktop composition starts child binaries as processes, while a Play-distributed Android app should not download and execute arbitrary binaries, DEX, or native libraries. Android explicitly warns that remote dynamic code loading can violate Play policy and creates code-substitution risk ([Android dynamic code loading guidance](https://developer.android.com/privacy-and-security/risks/dynamic-code-loading)).

## Proposed system boundary

The app should consist of one trusted Android host, one embedded Hugr runtime, a built-in orchestrator definition, and data-only marketplace packages. The host is responsible for every effect with authority: networking, Android storage, account access, notifications, microphone use, location, document selection, service credentials, model transport, scheduling, and user approval.

The preferred runtime direction is an Android library exposing Hugr sessions to Kotlin. It can be implemented by compiling the Rust core and necessary pure runtime components as an Android native library behind a small JNI interface, or by embedding a WebAssembly runtime and using the existing JSON-oriented WASM boundary. JNI avoids shipping a second virtual machine and can expose streaming with less serialization, while WASM gives marketplace logic a clearer portable sandbox. A short prototype should measure binary size, startup time, memory, cancellation behavior, trace throughput, and binding maintenance before choosing. In either case, Kotlin owns lifecycle and IO; no Android dependency enters `hugr-core`.

The runtime should support many isolated session actors. Each actor serializes events for one chat or delegated agent session, while the Kotlin host runs model and capability operations concurrently. Parallel chats are separate actors with separate budgets and cancellation scopes. A single chat may have concurrent host operations only where Hugr policy permits them. The core remains single-threaded per session.

The built-in orchestrator is a normal versioned agent definition with a mobile-specific turn policy. It receives only the tool cards for plugins enabled in the chat's pinned preset. Each plugin is exposed as an `agent_<id>` capability taking a Hugr `Ask` and returning the complete `Answer`. The host runs the child in-process under the child's own grants and limits, records parent-child lineage, and folds delegated cost into the parent answer. It does not share the orchestrator's capabilities, secrets, context, or scratch space with the child.

In-process delegation needs a mobile resolver to replace the current desktop subprocess resolver. It must create a child runtime from a verified package, map the child's declared capabilities to a restricted registry, supply only explicitly attached blobs, enforce resource limits, and return the child's answer. The resolver must prevent recursive or cyclic delegation from escaping depth, call, time, token, and cost limits.

## Agent package format

The marketplace needs a versioned mobile agent bundle distinct from a desktop executable. The bundle should be deterministic to build, content-addressed, signed, and inspectable before installation. A package should contain at least:

- Stable package id, display name, publisher id, semantic version, minimum host/runtime version, supported architectures if relevant, and package format version.
- A plain-language description, categories, icon assets, locale metadata, support and privacy links, source and license information, and marketplace maturity status.
- The system prompt, optional structured response schema, pure policy configuration, model selectors, context limits, and default budgets.
- Capability declarations with machine-readable schemas, privilege scopes, Android permission implications, foreground/background eligibility, data-use labels, approval policy, and user-facing reasons.
- External service declarations including OAuth provider, requested scopes, callback configuration, allowed network origins, credential type, and whether the service supports test or read-only modes.
- Optional portable modules in a deliberately constrained format. They may transform data or implement pure policy, but must have no ambient IO, reflection, dynamic linking, process access, or undeclared imports. If the Play distribution cannot safely support downloaded executable WASM, marketplace packages must remain data-only and all custom computation must run through reviewed host capabilities or a remote service.
- Integrity information for every file, the publisher signature, marketplace countersignature, build provenance, source revision, dependency inventory, review status, revocation information, and a reproducibility claim where available.
- Compatibility tests, schema fixtures, expected command traces, resource estimates, migration declarations, and package release notes.

The marketplace can accept source agent crates and build them in a controlled service, but the output for Android must be the mobile bundle, not the crate's desktop binary. The build service should pin toolchains, isolate builds, produce provenance and software bills of materials, scan prompts and dependencies, run replay and policy tests, and reject packages that require capabilities unavailable on mobile. Reproducible builds allow independent verification, but do not replace review.

Package installation should download to app-private storage, verify hashes and both signatures, verify compatibility and revocation status, inspect declarations, show the requested access to the user, then atomically activate the version. Keep the previous known-good version for rollback. A running chat remains pinned to its installed version; an update affects new chats unless the user explicitly migrates an existing chat.

## Kotlin and Java surface

The Android library should expose stable, asynchronous Kotlin APIs and Java-compatible facades. The public boundary should use generated or hand-maintained data classes for agent descriptors, package manifests, asks, answers, trace headers, streaming output events, capability requests, permission requests, usage, costs, and errors. Opaque JSON values should remain opaque where the Hugr brain does not branch on them.

The essential interfaces are a runtime factory, session handle, model adapter, capability implementation, storage backends, package resolver, permission broker, and event sink. A session should support start, resume, submit user input, observe streamed deltas and tool activity, approve or deny a pending action, interrupt, checkpoint, and close. Cancellation must propagate from Kotlin coroutines to model calls, capabilities, and child sessions, then return an explicit cancellation event to the brain.

The binding must never expose raw pointers or require callers to drive Android work from Rust threads. Calls into the core should be short and serialized. Host commands should be returned to Kotlin for asynchronous dispatch, and completed operations should enter through typed events. JNI failures, malformed JSON, unknown package versions, and process death must become recoverable host errors rather than corrupting traces.

The library also needs testing utilities: a fake model, fake capabilities, an in-memory store, deterministic time injection, trace verification, package fixture loading, and APIs for asserting command sequences. This makes marketplace compatibility tests possible without Android services or network access.

## Android capability layer

Desktop capabilities such as `fs_read`, `shell`, and unrestricted `web_fetch` cannot be copied unchanged. Android capabilities should describe user-level actions, use platform APIs, and expose the smallest useful schema. Capability registration is the effective sandbox: a plugin sees only implementations granted to that plugin in that preset.

Storage should use several distinct capabilities rather than a general path-based filesystem:

- Private scratch operations scoped to one plugin and chat, backed by app-private storage or a database. Names are logical paths and cannot escape the assigned root.
- Shared attachment and blob operations using content-addressed objects, encrypted at rest where appropriate, with explicit handles passed between parent and child.
- User-document operations mediated by the Storage Access Framework. A plugin receives access only to documents or directory trees the user selected, represented by persisted URI grants rather than raw paths.
- Notes, reminders, calendars, contacts, media, and downloads as typed domain capabilities using Android providers or app-owned stores, each with separate grants and write confirmations.

Network access should not be a generic socket. A host HTTP capability should enforce HTTPS by default, redirect and response-size limits, timeouts, metered-network preferences, private-address and loopback blocking, content-type rules, download quarantine, and a per-plugin origin allowlist. Authentication headers should be injected by a credential broker and never exposed to the model or plugin. Web research may require a separate search or reader service. Full browser automation is not generally available to an ordinary Android app and should not be implied by `web_fetch`.

Android integrations should be typed capabilities for location, maps intents, notifications, share sheets, clipboard, camera capture, photo selection, media, alarms, calendar, contacts, nearby devices, and accessibility only when the product has a defensible user-facing need. Runtime Android permissions are a platform prerequisite, not the complete Hugr grant: both the OS permission and the preset's plugin grant must allow an operation. Denial, temporary unavailability, and approximate data must be ordinary tool results the model can handle.

Third-party services such as Slack, GitHub, email, travel providers, or note systems should use dedicated connectors with OAuth Authorization Code plus PKCE where supported. Tokens belong in encrypted host-managed credential storage, keyed by account and connector, and are never placed in prompts, traces, package files, or tool results. Each connector should offer narrow operations and scopes, separate reads from writes, support account selection, show the acting identity, and revoke cleanly. High-impact or irreversible actions require an approval preview at execution time.

There should be no general shell capability in the Play app. A coding plugin can inspect user-selected repositories, propose patches, run remote CI, open branches, and create pull requests through a constrained GitHub connector or an explicitly configured remote workspace. It cannot compile arbitrary projects, execute repository scripts, manipulate other apps, or provide a trustworthy local terminal inside the standard mobile sandbox. A separately distributed developer edition could choose a different risk boundary, but it would be a different product and policy posture.

Capabilities need common operational rules: input validation, output-size limits, structured errors, timeouts, cancellation, audit events, retry classification, idempotency keys for writes, and redaction before trace persistence. Each declares whether it may run while the UI is absent and which Android foreground service type or permission it requires.

## Permissions and approvals

Permissions should be layered so no single consent grants broad ambient access. Installation accepts package identity and declared requirements. Enabling a plugin in a preset chooses its allowed capability scopes and accounts. Android asks for an OS permission only when a related action is first needed. The user then approves sensitive actions at execution time.

Read-only, reversible, and consequential operations should have different defaults. Reading an explicitly selected note may be allowed for the chat. Creating a draft can be allowed with notification. Sending a message, publishing code, purchasing, deleting, changing account permissions, revealing precise location, or sharing a private document should require a preview that names the plugin, account, destination, data, and consequence. The model cannot approve its own request, and a plugin cannot turn a denied operation into another broader capability.

Users need a permission dashboard showing effective grants by preset and plugin, connected accounts and scopes, recent use, background eligibility, data destinations, and quick revocation. Revocation cancels pending work where safe, invalidates credentials, and affects all future operations. Existing immutable traces remain but sensitive fields should have been excluded or encrypted according to retention policy.

Prompt injection must be treated as untrusted data crossing a security boundary. Web pages, Slack messages, repository files, documents, marketplace descriptions, plugin answers, and even note contents cannot grant capabilities or change approval policy. The orchestrator may use their content for reasoning, but authority comes only from host configuration and explicit user decisions.

## Orchestrator behavior

The orchestrator should be conservative and legible. It receives the user's message, conversation projection, available plugin cards, current budget, and relevant approval state. It can answer directly, ask the user for missing intent, call one or more specialists, or propose an action. It should explain delegation in the activity timeline without flooding the main conversation with internal reasoning.

Plugin discovery should happen outside the model context. The host indexes installed plugin metadata and presents only preset-enabled cards, possibly with a deterministic relevance filter when a preset contains many plugins. Sending an endless marketplace catalog to every model call would increase cost, reduce tool-selection accuracy, and expose plugins the user did not enable.

The orchestrator needs rules for selecting specialists, preserving their child trace ids, parallelizing independent read tasks, combining conflicting answers, handling partial failure, preventing loops, and stopping when limits are reached. A mobile policy should cap delegation depth, fan-out, model calls, wall time, tokens, cost, attachment bytes, and background duration. Limits apply to the entire task tree as well as each agent.

The user should be able to force a plugin, exclude one for a message, inspect which plugin handled a result, interrupt a task tree, and retry from a prior checkpoint. An orchestrator response must not conceal failed delegations or present an unconfirmed external write as completed.

## Presets and configuration

A preset should include only stable configuration, never raw access tokens. It references connector accounts through local opaque ids and records required scopes so presets can be shared without sharing credentials. A shared preset imports disabled until the recipient reviews unavailable plugins, model providers, estimated costs, data flows, and requested grants.

Useful built-in presets might include everyday assistant, travel planning, research, meeting notes, and GitHub review. A preset can choose default models independently for orchestration and specialists, maximum spend per chat and per day, whether metered data is allowed, whether tasks may continue in the background, a default voice configuration, retention, and approval strictness. Plugin-specific settings remain namespaced and schema-validated.

Configuration resolution should be deterministic: host policy and legal restrictions override user global settings, which override preset defaults, which override package defaults. A chat stores the resolved snapshot and package hashes. The UI should show conflicts before starting and never silently broaden access to make a preset work.

## Chat execution and persistence

Room is a suitable source of truth for chat metadata, execution state, pending operations, preset snapshots, trace indexes, and outbox records. Large trace payloads and blobs can live as encrypted app-private files referenced by content hash. Credentials require Android Keystore-backed encryption and a separate lifecycle. Search indexes, analytics, backups, and exported traces must respect redaction and retention choices.

Each externally visible write should use a durable intent record with a stable idempotency key. Persist the intent before dispatch, persist the result before telling the brain it completed, and reconcile unknown outcomes after process death. Services that lack idempotency may require the user to inspect the remote state rather than blindly retry. This is necessary for messages, tickets, commits, payments, reservations, and destructive edits.

The task state machine should distinguish queued, waiting for constraints, running in foreground, continuing with a visible notification, waiting for approval, waiting for user input, paused by the system, retryable failure, terminal success, terminal error, and cancelled. The UI can reconstruct every chat from this durable state and a streamed event timeline.

Immutable Hugr traces should remain the diagnostic and replay record. Device-local replay verifies decisions from recorded events but must not repeat IO. Resuming after interruption is new execution linked to the prior trace, not mutation of history. Trace export should redact secrets by construction and warn that user content and tool results may still be sensitive.

## Background and parallel work

Closing the screen does not guarantee unlimited execution. Android restricts background services, can kill the process, and may defer work based on battery, network, user settings, and manufacturer policy. WorkManager is appropriate for durable, deferrable, retryable jobs. Long-running workers use a foreground service and must display a notification; current Android versions also impose foreground-service start, service-type, duration, and job-quota restrictions ([background task overview](https://developer.android.com/develop/background-work/background-tasks), [long-running workers](https://developer.android.com/develop/background-work/background-tasks/persistent/how-to/long-running), [foreground service restrictions](https://developer.android.com/develop/background-work/services/fgs/restrictions-bg-start)).

When a user starts a task while the app is visible and asks it to continue, the host may promote it to a foreground service with a persistent notification showing the chat, current phase, cost, and stop action. Short network waits can complete there. Deferrable downloads, indexing, synchronization, and scheduled preset runs belong in WorkManager with constraints. A task that needs fresh microphone, camera, or while-in-use location access must return to foreground user interaction; it cannot assume those sensors remain available after the app is closed.

Parallel chats should be logically unlimited but physically scheduled. A global resource manager enforces small concurrency pools for model calls, downloads, local inference, and connectors, with fairness and per-provider rate limits. It should serialize conflicting writes, pause low-priority work under thermal or battery pressure, and expose queued state. Memory-heavy local models may force all local inference to run one at a time.

Background execution is checkpointed and resumable, not immortal. Device reboot, force-stop, revoked permission, logout, app update, quota exhaustion, or a provider outage can pause it. The notification and chat history must report that state honestly. Force-stopped apps cannot promise to restart themselves until the user opens them again.

## Voice mode

Voice should be an input and output transport around the same chat and approval model, not a separate agent. Its pipeline is microphone capture, voice activity detection, speech-to-text, user-visible transcript, orchestrator execution, streamed text, and text-to-speech playback. The transcript remains editable before sending in push-to-talk mode. Sensitive actions still require a visual or strongly authenticated confirmation; spoken model output must never count as user approval.

The application should support configurable speech-to-text and text-to-speech backends:

- The Android system recognizer and synthesizer provide a small integration and may offer on-device recognition where the installed system service supports it. Android exposes `createOnDeviceSpeechRecognizer`, but availability must be checked at runtime ([SpeechRecognizer API](https://developer.android.com/reference/android/speech/SpeechRecognizer)). A system provider may still have its own privacy and network behavior.
- Bundled or downloadable on-device models provide the clearest offline option and keep audio on the device, but increase app or asset size, RAM, battery use, startup time, language-specific testing, and device compatibility requirements.
- Remote speech services often provide better language coverage and streaming quality on weaker devices, but send audio to a provider, require network access, add latency and cost, and need explicit retention and regional disclosures.
- A hybrid default can prefer an available on-device backend, then ask before falling back to a configured remote provider. Users should be able to require local-only, remote-only, or automatic selection separately for recognition and synthesis.

Voice settings belong partly at app level and partly in presets: language, voice, speaking rate, endpointing, interruption, automatic send, local-only requirement, headset behavior, transcript retention, and whether responses should be read aloud in the background. Provider credentials stay in the host credential store. The UI must indicate when the microphone is active and whether audio is leaving the device.

Continuous hotword listening should not be a default promise. It has substantial battery and privacy costs and is constrained by Android background microphone rules. Push-to-talk and an explicitly started, visible voice session are realistic. Supporting system-wide assistant behavior would require a separate Android role and product design, such as a `VoiceInteractionService`, and should not be assumed for the marketplace host.

## Local and remote language models

Model execution should also be configurable by selector. Remote models offer broader capability and lower device requirements but expose prompts and selected context to a provider. Local models improve offline operation and data locality but are constrained by memory, storage, thermal throttling, battery, context length, tool-calling quality, and model licensing. Many devices will not run a model capable of reliable multi-agent orchestration.

A practical design supports local, remote, and hybrid selectors with capability metadata. The host resolves a logical selector against device support, user privacy requirements, network state, preset budget, and package minimums. It must not silently send a local-only chat to a remote provider. Model cards should state context size, tool-call support, structured-output support, download size, expected memory, languages, and data policy.

On-device model files should be signed and content-addressed, downloaded through a managed asset path, evicted independently of agent packages, and subject to charging and unmetered-network preferences. Local inference belongs in a bounded service with cancellation, thermal response, and memory-pressure handling. Remote adapters should stream, retry transport failures safely, enforce certificate and endpoint policy, and record provider usage for cost accounting.

End-to-end local operation is possible only when the orchestrator model, every selected specialist model, speech stack, and required capability are local. A local model does not make a Slack, weather, web, maps, or GitHub task offline, and it does not make retrieved remote data private.

## Marketplace service

The marketplace needs more than file hosting. Its catalog service supports search, categories, compatibility filters, package manifests, version histories, screenshots, publisher pages, presets, and localized descriptions. The artifact service provides content-addressed downloads, signatures, deltas where useful, availability by region and host version, and a revocation feed. The identity service verifies publishers and protects signing keys. The review service combines automated analysis, capability-specific testing, reproducible builds, malware and prompt-injection review, privacy review, and human escalation.

Marketplace listings should display the plugin's exact capabilities, accounts and OAuth scopes, network destinations, model requirements, expected costs, local/remote data paths, background eligibility, source availability, last review, version history, and known limitations before installation. Ratings should be tied to verified versions and kept separate from security review.

The marketplace needs policies for impersonation, misleading capability claims, credential collection, hidden data transfer, spam, unsafe advice, copyrighted content, vulnerable dependencies, abandoned packages, and prohibited automation. It also needs reporting, publisher appeals, emergency removal, signing-key rotation, staged rollout, rollback, and incident notification. Revocation should block new starts and require explicit handling for active chats; deleting an already installed package without preserving its trace decoder may make old traces harder to inspect.

Published presets introduce supply-chain composition. A preset publisher cannot grant permissions on a user's behalf, and installing a preset must independently verify every referenced plugin and pin compatible versions. The marketplace should calculate the union of capabilities and data destinations, flag conflicts, and prevent a benign preset update from silently adding a privileged plugin.

The system should support private catalogs for organizations. Enterprise controls may restrict publishers, plugins, models, connectors, retention, export, and maximum budgets; distribute managed presets; provide audit export; and disable user-created presets. Work profiles and mobile-device management require separate testing because storage and account visibility differ across profiles.

## Google Play acceptance

Google Play can accept this product concept, but acceptance depends on the plugin boundary and the behavior of the whole catalog. No design can guarantee approval because policies and review decisions change, and Google holds the app publisher responsible for code, SDKs, content, data use, permissions, foreground services, payments, and downloaded material.

The defensible Play Store version downloads agent definitions as content: prompts, schemas, metadata, capability requests, presets, signatures, and other non-executable resources. The installed app already contains the Hugr engine and every Android capability implementation. Enabling a plugin selects and configures code that Google received in the app bundle; it does not add Android behavior that was absent at review. This resembles downloadable chatbot configuration more than an app store inside an app.

The version most likely to be rejected downloads Rust executables, native libraries, DEX or JAR files, Python or JavaScript that can introduce behavior, unrestricted WASM modules, APKs, or self-update logic. Google's Android security guidance says to avoid dynamic code loading and warns that many remote forms violate Play policy ([Android dynamic code loading guidance](https://developer.android.com/privacy-and-security/risks/dynamic-code-loading)). Current Play policy examples also identify SDKs that download DEX or native executable code from outside Google Play as violations ([Google Play Developer Program Policy](https://support.google.com/googleplay/android-developer/answer/17105854)). Calling a file a plugin or compiling it to WASM does not by itself change that risk.

Downloaded interpreted or portable content is not automatically prohibited in every case, but it must not enable policy violations. The practical burden would include constraining imports and APIs, reviewing every package, preventing packages from changing the app's fundamental purpose, and showing that untrusted code cannot access Android or host authority. A data-only public format should therefore be the launch boundary. If pure portable modules remain a product goal, obtain written Play policy guidance before relying on them and submit a review build that makes the mechanism and sandbox easy to inspect.

The marketplace also makes the app responsible for user-generated and third-party content. It needs published rules, automated and human moderation, reporting, blocking, takedown, age controls where appropriate, publisher identity, intellectual-property handling, privacy disclosures, and an emergency revocation path. High-risk categories such as medical, financial, children, accessibility, VPN/proxy, device control, background location, and broad file access require category-specific policy review or exclusion.

Foreground work must have a user-visible, policy-appropriate purpose, use declared service types, and respect Android quotas. The app must not claim that background agents run indefinitely. Sensitive permissions must be necessary to visible product features, requested in context, described accurately in the Play data safety form, and unavailable to plugins without the separate Hugr grant.

Marketplace monetization needs a store-specific review. Selling digital plugins, presets, model credits, or subscriptions consumed in the app will generally engage Google Play Billing requirements and service fees, subject to program, region, and policy exceptions. Purchases of physical goods or external real-world services follow different rules. The product should keep entitlement resolution in the trusted host and never let a package choose a payment route.

The Play submission should include a reviewer account, a representative catalog containing low- and high-privilege examples, complete documentation of the package format and signature chain, a list of all host capabilities, demonstrations of grant and approval enforcement, moderation tools, data-flow disclosures, background notifications, and instructions for exercising remote providers. A staged closed test and a pre-submission policy conversation are appropriate before investing in an unrestricted public catalog.

The conclusion is conditional: a curated marketplace of signed declarative Hugr agents, all executed by reviewed host capabilities, is a plausible Play Store app. An open marketplace that downloads executable plugins with broad device access has a substantial rejection and removal risk.

## Transposing the design to iPhone and iPad

The design is portable to iOS and iPadOS because the Hugr brain has no Android dependencies. The marketplace package, ask-and-answer contract, orchestrator, presets, schemas, signatures, trace format, delegation rules, limits, and most connector protocols can be shared. The host, bindings, storage, permissions, background scheduling, voice, notifications, local inference, and device capabilities must be implemented for Apple platforms.

The shared Rust core can be compiled into a signed XCFramework with a narrow C ABI and a Swift concurrency wrapper. This is the closest equivalent to the proposed JNI route and keeps one reducer implementation across platforms. Embedding a WASM runtime is another option, but it adds binary size and a second runtime without solving App Store restrictions on downloaded functionality. Swift should own IO and actor isolation, dispatch Hugr commands asynchronously, and submit typed completion events. Swift package generation, ABI stability, memory ownership, cancellation, streaming callbacks, and crash containment need dedicated binding tests.

Core Data, SQLite, or a Swift-native database can mirror Room's durable task state. App-container files can hold content-addressed blobs and encrypted traces, while Keychain and Secure Enclave-backed keys protect connector credentials. User documents should use document pickers, security-scoped URLs, and coordinated file access. Photos, contacts, calendar, reminders, location, notifications, microphone, camera, HealthKit, HomeKit, and Shortcuts each need dedicated capabilities, entitlements, usage descriptions, and review justification. Android URI grants, intents, WorkManager, services, Keystore, and providers have no direct portable API and must not leak into package contracts.

iOS background execution is more constrained and system-directed. `BGTaskScheduler` can schedule refresh, processing, and supported continued-processing work, but the system decides when many tasks run and can reject or expire requests ([BGTaskScheduler](https://developer.apple.com/documentation/backgroundtasks/bgtaskscheduler)). Background `URLSession` handles long uploads and downloads while the app is suspended ([background downloads](https://developer.apple.com/documentation/foundation/downloading-files-in-the-background)). A short user-started task may request limited continuation, while audio, location, VoIP, and other background modes must be used only for their declared purposes. Unlimited chatbot reasoning in the background is not a valid assumption.

For reliable long autonomous jobs, the iOS app should checkpoint locally and optionally hand execution to a declared cloud host. Push notifications can report progress and return the user to approvals, but must not contain confidential content. Several chats can remain logically active, while the device runs only the work iOS permits. A cloud handoff must preserve package and preset versions, trace lineage, budgets, credentials, and the visible device-versus-cloud execution label.

Voice maps to `AVAudioSession`, the Speech framework, and `AVSpeechSynthesizer`, or to bundled and remote providers. Apple's recognizer reports whether a locale supports on-device recognition, and a request can require local recognition only when that support exists ([on-device speech support](https://developer.apple.com/documentation/speech/sfspeechrecognizer/supportsondevicerecognition)). Push-to-talk and a visible voice session remain the reasonable default. Siri and App Intents can expose selected, predictable actions, but do not turn the app into an unrestricted always-listening assistant.

Local language and speech models can use Core ML or another app-bundled inference runtime, subject to device memory, storage, thermal behavior, licensing, and App Store package rules. Models downloaded as data and interpreted by code already in the app need integrity checks and a stable reviewed purpose. The same local-only, remote-only, and hybrid privacy settings should exist on both platforms, even if device capability detection differs.

A shared product repository should separate four layers: the platform-neutral Hugr runtime and mobile package specification, a Kotlin Android host, a Swift Apple host, and platform-independent marketplace services. Capability names and JSON schemas can be common where semantics match, but a package must declare platform availability and capability alternatives. Presets should report missing plugins or capabilities instead of silently substituting broader access.

## Apple App Store acceptance

Apple's current guidelines make this concept possible, but the iOS package boundary and catalog operations need to be designed around App Review from the start. Guideline 2.5.2 generally prohibits downloading, installing, or executing code that introduces or changes app functionality. Guideline 4.7 specifically allows certain non-embedded HTML5 and JavaScript mini apps, streaming games, chatbots, plug-ins, and emulator content under additional rules ([App Review Guidelines](https://developer.apple.com/app-store/review/guidelines/)). This is permission with conditions, not blanket approval for an arbitrary agent runtime.

The lowest-risk interpretation is again that Hugr agents are declarative chatbot configurations executed by fixed, reviewed host code. If packages contain executable WASM or another portable language, Apple may treat them as software under Guideline 4.7 or reject them under 2.5.2 unless the implementation fits an allowed category and technology. Apple's Mini Apps Partner Program describes hosted mini apps as packages or scripts written in HTML5, JavaScript, or another Apple-approved language, which does not establish that arbitrary WASM is accepted ([Mini Apps Partner Program](https://developer.apple.com/programs/mini-apps-partner/)). Written clarification from App Review is necessary before making downloaded executable modules fundamental to the iOS product.

Guideline 4.7 makes the host responsible for every offered plugin. The catalog must apply the privacy rules, filter objectionable material, provide reporting and timely response, allow abusive users to be blocked, apply age restrictions, and provide an index and metadata with universal links for all offered software. Guideline 4.7.2 says the app may not extend or expose native platform APIs to hosted software without Apple's prior permission. Guideline 4.7.3 requires explicit user consent for sharing data or privacy permissions with each hosted software item. These provisions support the document's host-owned capabilities and per-plugin consent model, but native integrations such as Contacts, Calendar, Photos, HealthKit, or HomeKit should be discussed with Apple before submission.

Apple's user-generated-content rules may also apply to community-authored agents and presets. The app needs moderation and contact mechanisms even if plugins contain prompts rather than conventional social posts. The app's age rating must cover catalog content and generated output. Reviewers need access to the live catalog and all material feature paths, and emergency catalog changes cannot be used to introduce behavior Apple could not review.

Digital plugin purchases, premium presets, subscriptions, and model credits consumed in the app normally require In-App Purchase under Guideline 3.1, with storefront-specific exceptions handled by legal and product review. Guideline 3.2.2 also rejects general-interest interfaces for third-party apps, extensions, or plug-ins that merely imitate the App Store. The product should be presented and built as one conversational service with a curated agent catalog, not as an alternative native app store. Its marketplace needs meaningful editorial, safety, permission, and orchestration functions beyond listing packages.

Background behavior must comply with Guideline 2.5.4 and the declared Apple background modes. A cloud-executed agent with push updates is easier to justify than keeping arbitrary local computation alive after the user leaves. Microphone, location, HealthKit, HomeKit, and other entitlement-backed integrations face both technical and review scrutiny, independent of plugin consent.

The App Review package should explain Hugr's fixed runtime, demonstrate that packages cannot call native APIs directly, provide the full catalog index, moderation controls, per-plugin consent, age controls, payments, sample accounts, cloud execution, and background behavior, and disclose any downloaded portable logic explicitly. Seeking App Review guidance for the plugin model and native capability broker before launch is warranted.

The conclusion is also conditional: a curated catalog of declarative agents inside a substantive chatbot application has a credible App Store path, and Apple's rules explicitly contemplate hosted chatbots and plug-ins. An open executable marketplace, unreviewed native-capability exposure, weak moderation, or an app that looks like a general alternative app store is likely to be rejected.

## Existing products and the remaining gap

As of July 11, 2026, several products cover parts of this idea. This is a current market scan, not a claim that every regional or early-stage app was found.

- ChatGPT has a GPT Store of purpose-specific configurations with instructions, knowledge, capabilities, apps, and actions. The mobile apps can use GPTs, although creation and editing remain web-only according to OpenAI's current documentation ([GPT creation and mobile availability](https://help.openai.com/en/articles/8554397)). This is the closest large-scale example of a chatbot catalog, but it is a hosted platform rather than a user-controlled Hugr runtime with device capability jails, portable traces, and presets composed from several independent agents.
- Poe offers Android and iOS clients, an ecosystem of custom bots, bot creation, multiple model providers, and chats that can combine bots. Its Google Play listing describes more than one million custom bots ([Poe on Google Play](https://play.google.com/store/apps/details?id=com.poe.android)). It validates demand for a large mobile bot catalog, but its bots are primarily hosted conversational services rather than locally packaged, capability-isolated mobile agents.
- Gemini on Android combines a main assistant with user-selectable Connected Apps, including Google services and third parties, and can perform device actions such as calls, messages, alarms, and settings changes with permission ([Gemini Connected Apps](https://support.google.com/gemini/answer/13695044)). Gems add customized assistants. This is close to the orchestrator-plus-connectors experience, but the catalog, runtime, models, and integrations are controlled by Google rather than an open marketplace of independently built agents.
- Qoder Mobile controls coding agents running on a desktop or in the cloud, keeps sessions active away from the phone, streams status, sends notifications, and lets users approve or redirect work ([Qoder Mobile](https://qoder.com/mobile)). It is a useful precedent for background agent supervision, but it is a companion for one coding platform rather than a general on-device plugin host.
- AgentOS is an iOS app whose App Store listing advertises a tool-using loop, more than 165 built-in tools, specialized agents, voice, device integrations, GitHub, cloud execution, and background operation through a relay ([AgentOS App Store listing](https://apps.apple.com/us/app/agentos-ai-agent-host/id6759534004)). It is notably close in breadth, but the public listing describes built-in tools and agents rather than a signed, independently published Hugr marketplace with portable packages and user-defined preset composition.
- Perplexity Spaces provide reusable custom instructions, files, selected models, and collaborative knowledge contexts ([Perplexity Spaces](https://hub-prod.perplexity.ai/hub/faq/what-are-spaces)). They resemble part of the preset and task-context model, but not a capability-bearing agent marketplace.

Adjacent automation products such as Tasker and Shortcuts show that users value configurable mobile actions, while remote agent companions show that long tasks can be supervised from a phone. They do not supply the combined Hugr properties proposed here.

The apparent gap is not another mobile chatbot with named personalities. It is a cross-platform host where third parties publish inspectable agent packages, users compose several agents into pinned presets, every agent receives a narrow capability jail, local and remote execution are explicit, multiple task trees remain traceable and resumable, and cost and lineage are part of every answer. The closest products validate individual pieces, but this review did not find a mainstream product that combines all of them under an open, portable runtime contract.

## Security and privacy architecture

The threat model includes malicious publishers, compromised marketplace infrastructure, tampered downloads, prompt injection in external content, over-broad plugins, confused-deputy delegation, credential exfiltration, abusive orchestration loops, poisoned updates, vulnerable model endpoints, another app sending crafted intents, rooted devices, and accidental disclosure through logs, backups, notifications, screenshots, or exported traces.

The primary controls are signed immutable packages, app-private storage, verified transport, no arbitrary downloaded Android code, capability registration from effective grants, per-plugin identity and storage, scoped OAuth tokens, origin allowlists, schema validation, explicit approvals, bounded execution, complete audit events, redaction, safe update pinning, and rapid revocation. Android exported components should default to false, incoming links and intents require strict validation, and notification contents should hide sensitive text by default.

Secrets must never enter model context unless the external protocol itself makes that unavoidable, and even then a host connector should perform the authenticated exchange. Tool results should return handles or minimal fields, not bearer tokens or full credential-bearing responses. Logs should redact headers, query secrets, document contents where not needed, and voice audio. Crash reporting and product analytics should be opt-in or privacy-preserving and logically separate from agent traces.

Data controls should cover per-chat retention, automatic deletion, encrypted backup eligibility, export, account deletion, marketplace telemetry, model-provider retention, connector data use, and voice recording. The app should provide a data-flow view per operation: what left the phone, which provider received it, which plugin requested it, and why.

Marketplace signatures prove origin, not safety. A reviewed plugin can still cause harm through authorized tools or bad model decisions. Approval design, narrow connectors, limits, audits, and user-visible attribution remain necessary after signature verification.

## Reliability, observability, and support

The user-facing activity timeline should show model work, plugin delegation, capability use, approvals, retries, pauses, cost, and completion without exposing private chain-of-thought. Each item links to the responsible plugin and account. Users can interrupt one operation, one chat, or all background work.

Developers need structured host logs, trace replay and verification, package and preset hashes, model and connector versions, Android device constraints, latency and usage metrics, crash correlation ids, and a sanitized diagnostic export. Diagnostics must distinguish model transport failure, semantic tool failure, permission denial, package incompatibility, background restriction, process death, and corrupted local state.

Compatibility testing should cover supported Android API levels, vendors with aggressive battery management, offline and captive networks, process death at every operation boundary, device reboot, low storage, low memory, thermal throttling, permission revocation, expired OAuth, package rollback, model upgrade, locale and accessibility, multi-window, and concurrent chats. Security testing should fuzz package parsing and schemas, attempt path and URI escapes, inject hostile documents and web pages, test connector scope boundaries, and verify that replay never repeats effects.

The product needs service-level limits and circuit breakers even when users select generous budgets. A runaway agent tree can consume money, battery, provider quota, and external API rate limits. The host should stop predictably and return a partial answer with the reason and recoverable next actions.

## What the design makes possible

- A single Android chat interface extended by an open-ended catalog of declarative specialist agents.
- Per-chat selection of built-in, marketplace, shared, organizational, or user-defined presets.
- Independent plugin versions, settings, model choices, accounts, grants, budgets, and retention policies.
- Several durable chats progressing concurrently within Android scheduling and resource limits.
- Foreground continuation with a visible notification and checkpointed deferred work that resumes after ordinary process death.
- In-process specialist delegation through one ask-and-answer contract, with isolated context, tools, storage, traces, and cost accounting.
- Local, remote, or hybrid language, speech recognition, and speech synthesis providers when the device and plugin requirements permit them.
- Offline conversations and local tools under a fully local preset, with no false claim that remote integrations work offline.
- Android-native notes, documents, media, location, calendar, notification, share, and account capabilities with explicit scopes.
- Third-party integrations such as Slack and GitHub through narrow, host-managed connectors and OAuth grants.
- Coding workflows that inspect selected content, propose changes, and use a remote GitHub or workspace service for execution and pull requests.
- Signed updates, version pinning, rollback, revocation, private catalogs, managed presets, audit export, and trace-based debugging.
- User inspection of which agent acted, which data it accessed, where data went, what it cost, and which actions remain pending.

## What the design does not make possible

- Downloading arbitrary Rust binaries, native libraries, DEX, JAR files, scripts, or unrestricted third-party code into the Play app and executing them as trusted plugins.
- Letting marketplace packages call Android APIs, open sockets, read arbitrary filesystem paths, access credentials, or inherit the orchestrator's privileges directly.
- An actually endless number of active tools in one model context. The catalog may be large, but each preset and turn must expose a bounded, relevant tool set.
- Guaranteed uninterrupted execution after the app is closed. Android may defer or stop work, and long work requires the appropriate scheduler or a user-visible foreground service.
- Background access to microphone, camera, precise location, or other while-in-use resources whenever an agent wants it.
- Silent messages, purchases, publications, deletions, account changes, or other consequential actions merely because a plugin requested them.
- A general local shell, unrestricted browser automation, arbitrary repository code execution, or desktop-equivalent coding environment in the standard Play-distributed app.
- Perfect isolation between malicious portable code and the host unless the portable format, runtime, imports, metering, and memory boundaries are formally constrained and tested. A data-only package format has the safer default.
- A guarantee that signed or reviewed plugins are correct, unbiased, secure, or safe for every use. Models and external services remain fallible.
- A guarantee of privacy from choosing a local orchestrator when a specialist, connector, speech backend, or capability still sends data remotely.
- Bit-for-bit reproduction of external effects. Hugr replay reproduces decisions from recorded events; it does not rerun Slack sends, web requests, purchases, or other IO.
- Transparent plugin or preset upgrades inside a running chat. Reproducibility requires chats to stay pinned until the user chooses a migration.
- Cross-plugin access to private scratch, traces, attachments, or accounts without an explicit host-mediated handoff.
- Use of Android runtime permission dialogs as the only security boundary. Hugr grants, connector scopes, host validation, and action approvals remain required.
- Reliable unattended hotword listening as an ordinary background feature, or system-assistant status without implementing and qualifying for the relevant Android role.

## Product decisions that must remain explicit

The largest architectural decision is whether marketplace packages are strictly declarative or may include sandboxed portable logic. Strictly declarative packages limit expressiveness but fit Android distribution and review much better. Portable WASM can support custom pure policies and transforms, but it adds a second untrusted-code boundary and may still require policy review for Play distribution. Arbitrary native or JVM download should remain out of scope.

The second decision is the execution product boundary. Device-only execution maximizes local control but cannot provide unrestricted coding, durable server-side jobs, or broad connector automation when the phone is offline. An optional cloud worker can provide those features, but it becomes a separate host with its own credentials, storage, billing, data residency, sandbox, trace synchronization, and trust disclosures. The UI must say where each task runs.

The third decision is marketplace governance. An open submission model without signatures, review, revocation, and capability-specific policy would turn the host into a confused deputy. A curated public catalog plus private developer sideloading is a more defensible initial boundary. Developer mode should be visibly separate, require explicit activation, use isolated data and accounts where possible, and never weaken the production marketplace's guarantees.

The fourth decision is voice and model defaulting. Local-first with an explicit remote fallback provides a clear privacy posture, but availability and quality vary by device and language. The application should make the selected route visible and let presets require a route rather than silently optimizing across the privacy boundary.

## Relationship to the current repository

The sans-IO reducer in `crates/hugr-core`, the host command/event contract, policies, traces, cost accounting, ask-and-answer contract, and browser WASM experience are reusable foundations. The Android product requires a new host and binding layer, Android storage backends, an in-process mobile agent resolver, a mobile bundle format and verifier, Android capability implementations, lifecycle-aware scheduling, a permission and credential broker, UI and persistence, voice and local-model adapters, and marketplace services.

The current standalone binary remains the right desktop and server artifact. It should not be stretched into the Android package format. Both formats can be produced from the same agent source when its declared tools and policy are portable, and the build service should report precisely when an agent depends on desktop-only capabilities.

The architecture should preserve the existing boundary: `hugr-core` remains pure and single-threaded, all nondeterminism enters as events, capability payloads remain opaque to the brain, traces remain immutable, and the host registers only explicitly granted capabilities. Android changes the host, packaging, and product surface, not those invariants.
