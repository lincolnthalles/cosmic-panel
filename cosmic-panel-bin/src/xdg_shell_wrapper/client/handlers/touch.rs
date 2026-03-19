use crate::xdg_shell_wrapper::server_state::{SeatPair, ServerPointerFocus};
use crate::xdg_shell_wrapper::shared_state::GlobalState;
use crate::xdg_shell_wrapper::space::WrapperSpace;
use sctk::delegate_touch;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::protocol::wl_touch::WlTouch;
use sctk::reexports::client::{Connection, QueueHandle};
use sctk::seat::touch::TouchHandler;
use smithay::input::touch::{self, TouchHandle};
use smithay::utils::{Point, SERIAL_COUNTER};

fn get_touch_handle(
    state: &GlobalState,
    touch: &WlTouch,
) -> Option<(String, TouchHandle<GlobalState>)> {
    let seat_index = state.server_state.seats.iter().position(|SeatPair { client, .. }| {
        client.touch.as_ref().map(|t| t == touch).unwrap_or(false)
    })?;
    let seat_name = state.server_state.seats[seat_index].name.to_string();
    let touch = state.server_state.seats[seat_index].server.seat.get_touch()?;
    Some((seat_name, touch))
}

impl TouchHandler for GlobalState {
    fn down(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        touch: &WlTouch,
        serial: u32,
        time: u32,
        surface: WlSurface,
        id: i32,
        location: (f64, f64),
    ) {
        let Some(seat_index) = self.server_state.seats.iter().position(|SeatPair { client, .. }| {
            client.touch.as_ref().map(|t| t == touch).unwrap_or(false)
        }) else {
            tracing::warn!("Dropping touch down event for unknown seat");
            return;
        };
        let seat_name = self.server_state.seats[seat_index].name.to_string();
        let Some(touch) = self.server_state.seats[seat_index].server.seat.get_touch() else {
            tracing::warn!("Dropping touch down event without server touch handle");
            return;
        };
        self.server_state.seats[seat_index].client.last_touch_down = (serial, time);

        self.client_state.touch_surfaces.insert(id, surface.clone());

        if let Some(ServerPointerFocus { surface, c_pos, s_pos, .. }) =
            self.space.touch_under((location.0 as i32, location.1 as i32), &seat_name, surface)
        {
            touch.down(self, Some((surface, s_pos)), &touch::DownEvent {
                slot: Some(id as u32).into(),
                location: c_pos.to_f64() + Point::from(location),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            });
            touch.frame(self);
        }
    }

    fn up(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        touch: &WlTouch,
        _serial: u32,
        time: u32,
        id: i32,
    ) {
        let Some((_, touch)) = get_touch_handle(self, touch) else {
            tracing::warn!("Dropping touch up event for unknown seat");
            return;
        };

        touch.up(self, &touch::UpEvent {
            slot: Some(id as u32).into(),
            serial: SERIAL_COUNTER.next_serial(),
            time,
        });
    }

    fn motion(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        touch: &WlTouch,
        time: u32,
        id: i32,
        location: (f64, f64),
    ) {
        let Some((seat_name, touch)) = get_touch_handle(self, touch) else {
            tracing::warn!("Dropping touch motion event for unknown seat");
            return;
        };

        if let Some(surface) = self.client_state.touch_surfaces.get(&id)
            && let Some(ServerPointerFocus { surface, c_pos, s_pos, .. }) = self.space.touch_under(
                (location.0 as i32, location.1 as i32),
                &seat_name,
                surface.clone(),
            )
        {
            touch.motion(self, Some((surface, s_pos)), &touch::MotionEvent {
                slot: Some(id as u32).into(),
                location: c_pos.to_f64() + Point::from(location),
                time,
            });
            touch.frame(self);
        }
    }

    fn shape(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &WlTouch,
        _id: i32,
        _major: f64,
        _minor: f64,
    ) {
        // TODO not supported in smithay
    }

    fn orientation(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &WlTouch,
        _id: i32,
        _orientation: f64,
    ) {
        // TODO not supported in smithay
    }

    fn cancel(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, touch: &WlTouch) {
        if let Some((_, touch)) = get_touch_handle(self, touch) {
            touch.cancel(self);
        } else {
            tracing::warn!("Dropping touch cancel event for unknown seat");
        }
    }
}

delegate_touch!(GlobalState);
