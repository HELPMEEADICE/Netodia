//! Keep the Windows thumbnail toolbar attached to the current Slint window.
use std::sync::Arc;
use std::time::Duration;

use netodia_app::state::AppState;
use netodia_core::error::describe;
use netodia_engine::player::engine::event_sink::{EventSink, PlayerEvent, PlayerSinks};
use netodia_engine::player::engine::state::PlayerViewModelLight;
use netodia_platform::services::platform::taskbar::{self, Action, Snapshot};
use netodia_ui::AppWindow;
use slint::ComponentHandle;

use super::event_sink::SlintEventSink;

pub fn install(ui: &AppWindow, state: &AppState) {
    let sink = Arc::new(SlintEventSink { state: state.clone() });
    let callback: Arc<dyn Fn(Action) + Send + Sync> = Arc::new(move |action| {
        sink.handle(match action {
            Action::Previous => PlayerEvent::Previous,
            Action::PlayPause => PlayerEvent::PlayPause,
            Action::Next => PlayerEvent::Next,
        });
    });
    let weak = ui.as_weak();
    let subscriber_callback = callback.clone();
    let mut rx = state.sinks.view_model.subscribe();
    if let Err(e) = slint::spawn_local(async_compat::Compat::new(async move {
        loop {
            // Prime from the current value too: subscribing marks it as already seen.
            let snapshot = snapshot(rx.borrow_and_update().as_ref());
            let Some(ui) = weak.upgrade() else { break };
            update(&ui, snapshot, &subscriber_callback);
            if rx.changed().await.is_err() {
                break;
            }
        }
    })) {
        log::warn!("taskbar: state subscriber failed to spawn: {}", describe(&e));
    }
    // A window can be recreated without a playback change, and Explorer can reject an update.
    tick(ui.as_weak(), state.sinks.clone(), callback);
}

fn snapshot(vm: Option<&PlayerViewModelLight>) -> Snapshot {
    vm.map_or_else(Snapshot::default, |vm| Snapshot {
        playing: vm.status == "playing",
        has_previous: vm.has_previous,
        has_next: vm.has_next,
    })
}

fn update(ui: &AppWindow, snapshot: Snapshot, callback: &Arc<dyn Fn(Action) + Send + Sync>) {
    if let Some(hwnd) = crate::ui::window_chrome::win32_hwnd(ui) {
        taskbar::update(hwnd, snapshot, callback.clone());
    }
}

fn tick(
    weak: slint::Weak<AppWindow>,
    sinks: Arc<PlayerSinks>,
    callback: Arc<dyn Fn(Action) + Send + Sync>,
) {
    slint::Timer::single_shot(Duration::from_millis(500), move || {
        let Some(ui) = weak.upgrade() else { return };
        let snapshot = snapshot(sinks.view_model.borrow().as_ref());
        update(&ui, snapshot, &callback);
        tick(weak, sinks, callback);
    });
}
