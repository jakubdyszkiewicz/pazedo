# Pazedo

This repository is Pazedo, a fork of Zed that is a client for Paseo (https://paseo.sh): Paseo's
daemon owns workspaces, agents and terminals, and Pazedo shows them in Zed. Start with
`README.md`, then `docs/pazedo/README.md` (fork layout, upstream sync, every change to Zed's own
code) and `docs/pazedo/development.md` (building, running, every feature and how it was
verified). Open work is in `docs/pazedo/todo.md`.

## Where code goes

* Fork code lives in its own crates: `crates/paseo_client` (the daemon protocol, no gpui),
  `crates/paseo_ui` (sidebar, panel, agent and terminal tabs, usage indicator) and
  `crates/type_ahead`. Fork tooling lives in `script/pazedo/`.
* Change Zed's own crates only where there is no seam, keep the change small, mark it with a
  `// Pazedo:` comment, and add it to "Upstream touch points" in `docs/pazedo/README.md`. Prefer a
  hook in Zed (as `TitleBar::add_item` or `Item::tab_extra_context_menu_actions`) over copying a
  Zed component.
* The protocol's source of truth is the Zod schemas of `@getpaseo/protocol` (the version is
  `PASEO_PROTOCOL_PACKAGE_VERSION` in `crates/paseo_client/src/protocol.rs`) and Paseo's own
  source. Record what you verify against a daemon in `docs/pazedo/paseo-protocol.md`.
* Pazedo's default settings and keybindings go in `assets/settings/default.json` and at the end
  of `assets/keymaps/default-macos.json`.

## Paseo daemons: what is safe

* The user's own daemons run their real agents. Never stop or restart them, and never message,
  cancel, answer permissions for, archive or close their agents or terminals. Closing an agent
  tab in Pazedo archives the agent, and closing a terminal tab kills the terminal. Read-only
  requests are fine.
* Experiment on the test daemon instead: `script/pazedo/paseo-test` runs Paseo's CLI against
  an isolated daemon (127.0.0.1:6868, home `~/.paseo-pazedo-test`) with the `mock` provider.
  See "Paseo test daemon" in `docs/pazedo/development.md`. Check the port with `lsof` before
  signalling any daemon process.

## Building and verifying

* Iterate with debug builds (`cargo build -p zed`). Make release builds
  (`script/pazedo/bundle`, about 30 minutes) only when asked.
* `script/pazedo/run-dev` starts the dev build against the test daemon, with its own data
  directory. Never start a dev build without a separate data directory while Pazedo.app runs:
  they would share settings and databases.
* Check UI changes in the running dev build, not only in tests: `script/pazedo/screenshot`
  captures the window, and `script/pazedo/ui` clicks, types and presses keys. Do not answer
  macOS system or permission dialogs.
* Run `cargo test -p paseo_ui -p paseo_client` and `./script/clippy` on the crates you changed.
* Document each feature in `docs/pazedo/development.md` when it lands, including what was
  verified, how, and when. Say plainly what was not verified.

## Traps

* macOS reports a shifted key as its character: cmd-shift-0 arrives as `cmd-)` and
  cmd-shift-minus as `cmd-_`. A binding written as `cmd-shift-0` never matches.
* Keybindings registered in code are dropped when Zed reloads the keymap. Put them in
  `assets/keymaps/default-macos.json`.
* An action from a tab's context menu does not reach an inactive tab, because Zed doesn't
  render inactive tabs. Put the target's id in the action and handle it app-wide with
  `cx.on_action`.
* The daemon sends no update for a closed agent. After it accepts a change (such as a rename),
  update the store yourself.

## Commits

* Work on `main`; commit finished, tested work. Write commit titles as imperative sentences,
  optionally prefixed with the crate (`paseo_ui: Show …`).
* Merge upstream Zed, don't rebase on it (see `docs/pazedo/README.md`).

# Rust coding guidelines

* Prioritize code correctness and clarity. Speed and efficiency are secondary priorities unless otherwise specified.
* Do not write organizational or comments that summarize the code. Comments should only be written in order to explain "why" the code is written in some way in the case there is a reason that is tricky / non-obvious.
* Prefer implementing functionality in existing files unless it is a new logical component. Avoid creating many small files.
* Avoid using functions that panic like `unwrap()`, instead use mechanisms like `?` to propagate errors.
* Be careful with operations like indexing which may panic if the indexes are out of bounds.
* Never silently discard errors with `let _ =` on fallible operations. Always handle errors appropriately:
  - Propagate errors with `?` when the calling function should handle them
  - Use `.log_err()` or similar when you need to ignore errors but want visibility
  - Use explicit error handling with `match` or `if let Err(...)` when you need custom logic
  - Example: avoid `let _ = client.request(...).await?;` - use `client.request(...).await?;` instead
* When implementing async operations that may fail, ensure errors propagate to the UI layer so users get meaningful feedback.
* Never create files with `mod.rs` paths - prefer `src/some_module.rs` instead of `src/some_module/mod.rs`.
* When creating new crates, prefer specifying the library root path in `Cargo.toml` using `[lib] path = "...rs"` instead of the default `lib.rs`, to maintain consistent and descriptive naming (e.g., `gpui.rs` or `main.rs`).
* Avoid creative additions unless explicitly requested
* Use full words for variable names (no abbreviations like "q" for "queue")
* Use variable shadowing to scope clones in async contexts for clarity, minimizing the lifetime of borrowed references.
  Example:
  ```rust
  executor.spawn({
      let task_ran = task_ran.clone();
      async move {
          *task_ran.borrow_mut() = true;
      }
  });
  ```

# Timers in tests

* In GPUI tests, prefer GPUI executor timers over `smol::Timer::after(...)` when you need timeouts, delays, or to drive `run_until_parked()`:
  - Use `cx.background_executor().timer(duration).await` (or `cx.background_executor.timer(duration).await` in `TestAppContext`) so the work is scheduled on GPUI's dispatcher.
  - Avoid `smol::Timer::after(...)` for test timeouts when you rely on `run_until_parked()`, because it may not be tracked by GPUI's scheduler and can lead to "nothing left to run" when pumping.

# GPUI

GPUI is a UI framework which also provides primitives for state and concurrency management.

## Context

Context types allow interaction with global state, windows, entities, and system services. They are typically passed to functions as the argument named `cx`. When a function takes callbacks they come after the `cx` parameter.

* `App` is the root context type, providing access to global state and read and update of entities.
* `Context<T>` is provided when updating an `Entity<T>`. This context dereferences into `App`, so functions which take `&App` can also take `&Context<T>`.
* `AsyncApp` and `AsyncWindowContext` are provided by `cx.spawn` and `cx.spawn_in`. These can be held across await points.

## `Window`

`Window` provides access to the state of an application window. It is passed to functions as an argument named `window` and comes before `cx` when present. It is used for managing focus, dispatching actions, directly drawing, getting user input state, etc.

## Entities

An `Entity<T>` is a handle to state of type `T`. With `thing: Entity<T>`:

* `thing.entity_id()` returns `EntityId`
* `thing.downgrade()` returns `WeakEntity<T>`
* `thing.read(cx: &App)` returns `&T`.
* `thing.read_with(cx, |thing: &T, cx: &App| ...)` returns the closure's return value.
* `thing.update(cx, |thing: &mut T, cx: &mut Context<T>| ...)` allows the closure to mutate the state, and provides a `Context<T>` for interacting with the entity. It returns the closure's return value.
* `thing.update_in(cx, |thing: &mut T, window: &mut Window, cx: &mut Context<T>| ...)` takes a `AsyncWindowContext` or `VisualTestContext`. It's the same as `update` while also providing the `Window`.

Within the closures, the inner `cx` provided to the closure must be used instead of the outer `cx` to avoid issues with multiple borrows.

Trying to update an entity while it's already being updated must be avoided as this will cause a panic.

`WeakEntity<T>` is a weak handle. It has `read_with`, `update`, and `update_in` methods that work the same, but always return an `anyhow::Result` so that they can fail if the entity no longer exists. This can be useful to avoid memory leaks - if entities have mutually recursive handles to each other they will never be dropped.

## Concurrency

All use of entities and UI rendering occurs on a single foreground thread.

`cx.spawn(async move |cx| ...)` runs an async closure on the foreground thread. Within the closure, `cx` is `&mut AsyncApp`.

When the outer cx is a `Context<T>`, the use of `spawn` instead looks like `cx.spawn(async move |this, cx| ...)`, where `this: WeakEntity<T>` and `cx: &mut AsyncApp`.

To do work on other threads, `cx.background_spawn(async move { ... })` is used. Often this background task is awaited on by a foreground task which uses the results to update state.

Both `cx.spawn` and `cx.background_spawn` return a `Task<R>`, which is a future that can be awaited upon. If this task is dropped, then its work is cancelled. To prevent this one of the following must be done:

* Awaiting the task in some other async context.
* Detaching the task via `task.detach()` or `task.detach_and_log_err(cx)`, allowing it to run indefinitely.
* Storing the task in a field, if the work should be halted when the struct is dropped.

A task which doesn't do anything but provide a value can be created with `Task::ready(value)`.

## Elements

The `Render` trait is used to render some state into an element tree that is laid out using flexbox layout. An `Entity<T>` where `T` implements `Render` is sometimes called a "view".

Example:

```
struct TextWithBorder(SharedString);

impl Render for TextWithBorder {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().border_1().child(self.0.clone())
    }
}
```

Since `impl IntoElement for SharedString` exists, it can be used as an argument to `child`. `SharedString` is used to avoid copying strings, and is either an `&'static str` or `Arc<str>`.

UI components that are constructed just to be turned into elements can instead implement the `RenderOnce` trait, which is similar to `Render`, but its `render` method takes ownership of `self` and receives `&mut App` instead of `&mut Context<Self>`. Types that implement this trait can use `#[derive(IntoElement)]` to use them directly as children.

The style methods on elements are similar to those used by Tailwind CSS.

If some attributes or children of an element tree are conditional, `.when(condition, |this| ...)` can be used to run the closure only when `condition` is true. Similarly, `.when_some(option, |this, value| ...)` runs the closure when the `Option` has a value.

## Input events

Input event handlers can be registered on an element via methods like `.on_click(|event, window, cx: &mut App| ...)`.

Often event handlers will want to update the entity that's in the current `Context<T>`. The `cx.listener` method provides this - its use looks like `.on_click(cx.listener(|this: &mut T, event, window, cx: &mut Context<T>| ...)`.

## Actions

Actions are dispatched via user keyboard interaction or in code via `window.dispatch_action(SomeAction.boxed_clone(), cx)` or `focus_handle.dispatch_action(&SomeAction, window, cx)`.

Actions with no data are defined with the `actions!(some_namespace, [SomeAction, AnotherAction])` macro call. Otherwise the `Action` derive macro is used. Doc comments on actions are displayed to the user.

Action handlers can be registered on an element via the event handler `.on_action(|action, window, cx| ...)`. Like other event handlers, this is often used with `cx.listener`.

## Notify

When a view's state has changed in a way that may affect its rendering, it should call `cx.notify()`. This will cause the view to be rerendered. It will also cause any observe callbacks registered for the entity with `cx.observe` to be called.

## Entity events

While updating an entity (`cx: Context<T>`), it can emit an event using `cx.emit(event)`. Entities register which events they can emit by declaring `impl EventEmitter<EventType> for EntityType {}`.

Other entities can then register a callback to handle these events by doing `cx.subscribe(other_entity, |this, other_entity, event, cx| ...)`. This will return a `Subscription` which deregisters the callback when dropped.  Typically `cx.subscribe` happens when creating a new entity and the subscriptions are stored in a `_subscriptions: Vec<Subscription>` field.

## Build guidelines

- Use `./script/clippy` instead of `cargo clippy`

# Rules hygiene

Keep this file high-signal: traps to avoid and rules that are easy to get wrong, not
descriptions of the code, which go stale (those belong in `docs/pazedo/`). Zed's guidance
above comes from upstream; when merging upstream changes to it, take them.
