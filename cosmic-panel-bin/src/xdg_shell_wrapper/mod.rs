// SPDX-License-Identifier: MPL-2.0
#![warn(missing_debug_implementations, missing_docs)]

//! Provides the core functionality for cosmic-panel

use std::{
    panic::AssertUnwindSafe,
    time::{Duration, Instant},
};

use anyhow::Result;
use sctk::shm::multi::MultiPool;
use smithay::reexports::calloop;
use smithay::reexports::wayland_server::Display;

pub use client::handlers::{wp_fractional_scaling, wp_security_context, wp_viewporter};
pub use client::state as client_state;
use client::state::ClientState;
pub use server::state as server_state;
use server::state::ServerState;
use shared_state::GlobalState;
use space::{Visibility, WrapperSpace};
pub use xdg_shell_wrapper_config as config;

use crate::space_container::SpaceContainer;

pub(crate) mod client;
mod server;
/// shared state
pub mod shared_state;
/// wrapper space abstraction
pub mod space;
/// utilities
pub mod util;

/// run the cosmic panel xdg wrapper with the provided config
pub fn run(
    mut space: SpaceContainer,
    client_state: ClientState,
    embedded_server_state: ServerState,
    mut event_loop: calloop::EventLoop<'static, GlobalState>,
    mut server_display: Display<GlobalState>,
) -> Result<()> {
    let start = std::time::Instant::now();

    let s_dh = server_display.handle();
    space.set_display_handle(s_dh.clone());

    let mut global_state = GlobalState::new(client_state, embedded_server_state, space, start);

    global_state.space.setup(
        &global_state.client_state.compositor_state,
        global_state.client_state.fractional_scaling_manager.as_ref(),
        global_state.client_state.security_context_manager.clone(),
        global_state.client_state.viewporter_state.as_ref(),
        &mut global_state.client_state.layer_state,
        &global_state.client_state.connection,
        &global_state.client_state.queue_handle,
        global_state.client_state.overlap_notify.clone(),
    );

    let multipool = MultiPool::new(&global_state.client_state.shm_state);

    let cursor_surface = global_state
        .client_state
        .compositor_state
        .create_surface(&global_state.client_state.queue_handle);
    global_state.client_state.multipool = multipool.ok();
    if let Some((scale, vp)) = global_state
        .client_state
        .fractional_scaling_manager
        .as_ref()
        .zip(global_state.client_state.viewporter_state.as_ref())
    {
        global_state.client_state.cursor_scale = Some(
            scale.fractional_scaling(&cursor_surface, &global_state.client_state.queue_handle),
        );
        global_state.client_state.cursor_vp =
            Some(vp.get_viewport(&cursor_surface, &global_state.client_state.queue_handle));
    }

    global_state.client_state.cursor_surface = Some(cursor_surface);

    match std::panic::catch_unwind(AssertUnwindSafe(|| {
        event_loop.dispatch(Duration::from_millis(30), &mut global_state)
    })) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return Err(err.into()),
        Err(_) => {
            tracing::error!("Recovered from panic during initial event-loop dispatch");
        },
    }

    let handle = event_loop.handle();
    handle
        .insert_source(
            calloop::timer::Timer::from_duration(Duration::from_secs(2)),
            |_, _, state| {
                state.cleanup();
                calloop::timer::TimeoutAction::ToDuration(Duration::from_secs(2))
            },
        )
        .expect("Failed to insert cleanup timer.");
    global_state.bind_display(&s_dh);

    // TODO find better place for this
    // let set_clipboard_once = Rc::new(Cell::new(false));

    let mut prev_dur = Duration::from_millis(16);
    let mut consecutive_dispatch_failures = 0_u32;
    const MAX_CONSECUTIVE_DISPATCH_FAILURES: u32 = 8;
    loop {
        let iter_start = Instant::now();

        let visibility = matches!(global_state.space.visibility(), Visibility::Hidden);
        // dispatch desktop client events
        let dur = if matches!(global_state.space.visibility(), Visibility::Hidden) {
            Duration::from_millis(300)
        } else {
            Duration::from_millis(16)
        }
        .max(prev_dur);

        match std::panic::catch_unwind(AssertUnwindSafe(|| event_loop.dispatch(dur, &mut global_state)))
        {
            Ok(Ok(())) => {
                consecutive_dispatch_failures = 0;
            }
            Ok(Err(err)) => {
                consecutive_dispatch_failures = consecutive_dispatch_failures.saturating_add(1);
                tracing::error!(
                    "Event-loop dispatch failed (attempt {consecutive_dispatch_failures}/{MAX_CONSECUTIVE_DISPATCH_FAILURES}): {err:?}"
                );
                if consecutive_dispatch_failures >= MAX_CONSECUTIVE_DISPATCH_FAILURES {
                    return Err(anyhow::anyhow!(
                        "Event-loop dispatch failed repeatedly; aborting to recover from stuck state"
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            },
            Err(_) => {
                consecutive_dispatch_failures = consecutive_dispatch_failures.saturating_add(1);
                tracing::error!(
                    "Recovered from panic during event-loop dispatch (attempt {consecutive_dispatch_failures}/{MAX_CONSECUTIVE_DISPATCH_FAILURES})"
                );
                if consecutive_dispatch_failures >= MAX_CONSECUTIVE_DISPATCH_FAILURES {
                    return Err(anyhow::anyhow!(
                        "Event-loop dispatch panicked repeatedly; aborting to recover from stuck state"
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            },
        }

        // rendering
        if std::panic::catch_unwind(AssertUnwindSafe(|| {
            let space = &mut global_state.space;
            let _ = space.handle_events(
                &s_dh,
                &global_state.client_state.queue_handle,
                &mut global_state.server_state.popup_manager,
                global_state.start_time.elapsed().as_millis().try_into()?,
                Some(dur),
            );
            Result::<()>::Ok(())
        }))
        .is_err()
        {
            tracing::error!("Recovered from panic in space event handling");
            continue;
        }

        if std::panic::catch_unwind(AssertUnwindSafe(|| {
            global_state.draw_dnd_icon();
        }))
        .is_err()
        {
            tracing::error!("Recovered from panic while drawing drag-and-drop icon");
            continue;
        }

        if let Some(renderer) = global_state.space.renderer()
            && std::panic::catch_unwind(AssertUnwindSafe(|| {
                global_state.client_state.draw_layer_surfaces(
                    renderer,
                    global_state.start_time.elapsed().as_millis().try_into()?,
                );
                Result::<()>::Ok(())
            }))
            .is_err()
        {
            tracing::error!("Recovered from panic while drawing proxied layer surfaces");
            continue;
        }

        // dispatch server events
        match std::panic::catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            server_display.dispatch_clients(&mut global_state)?;
            server_display.flush_clients()?;
            Ok(())
        })) {
            Ok(Ok(())) => {
                consecutive_dispatch_failures = 0;
            }
            Ok(Err(err)) => {
                consecutive_dispatch_failures = consecutive_dispatch_failures.saturating_add(1);
                tracing::error!(
                    "Server dispatch failed (attempt {consecutive_dispatch_failures}/{MAX_CONSECUTIVE_DISPATCH_FAILURES}): {err:?}"
                );
                if consecutive_dispatch_failures >= MAX_CONSECUTIVE_DISPATCH_FAILURES {
                    return Err(anyhow::anyhow!(
                        "Server dispatch failed repeatedly; aborting to recover from stuck state"
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            },
            Err(_) => {
                consecutive_dispatch_failures = consecutive_dispatch_failures.saturating_add(1);
                tracing::error!(
                    "Recovered from panic while dispatching server clients (attempt {consecutive_dispatch_failures}/{MAX_CONSECUTIVE_DISPATCH_FAILURES})"
                );
                if consecutive_dispatch_failures >= MAX_CONSECUTIVE_DISPATCH_FAILURES {
                    return Err(anyhow::anyhow!(
                        "Server dispatch panicked repeatedly; aborting to recover from stuck state"
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            },
        }
        global_state.iter_count += 1;

        let new_visibility_hidden = matches!(global_state.space.visibility(), Visibility::Hidden);

        if visibility != new_visibility_hidden {
            prev_dur = Duration::from_millis(16);
            continue;
        }
        if let Some(dur) = Instant::now()
            .checked_duration_since(iter_start)
            .and_then(|spent| dur.checked_sub(spent))
        {
            std::thread::sleep(dur.min(Duration::from_millis(if new_visibility_hidden {
                50
            } else {
                16
            })));
        } else {
            prev_dur = prev_dur.checked_mul(2).unwrap_or(prev_dur).min(Duration::from_millis(100));
        }
    }
}
