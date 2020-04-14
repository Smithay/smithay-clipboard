use sctk::reexports::client::protocol::wl_seat::WlSeat;

/// Data to track latest seat and serial for clipboard requests.
#[derive(Default)]
pub struct ClipboardDispatchData {
    observed_seats: Vec<(WlSeat, u32)>,
    last_pos: usize,
}

impl ClipboardDispatchData {
    /// Builds new `ClipboardDispatchData` with all fields equal to `None`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the last observed seat.
    pub fn set_last_seat(&mut self, seat: WlSeat, serial: u32) {
        let pos = self.observed_seats.iter().position(|st| st.0 == seat);
        match pos {
            Some(pos) => {
                // Update serial and set the last data we've seen.
                self.observed_seats[pos].1 = serial;
                self.last_pos = pos;
            }
            None => {
                // Add new seat and mark it as last.
                self.last_pos = self.observed_seats.len();
                self.observed_seats.push((seat, serial));
            }
        }
    }

    /// Remove the given seat from the observer seats.
    pub fn remove_seat(&mut self, seat: WlSeat) {
        let pos = self.observed_seats.iter().position(|st| st.0 == seat);

        if let Some(pos) = pos {
            // Remove the seat data.
            self.observed_seats.remove(pos);
        }
    }

    /// Return the last observed seat and the serial.
    pub fn last_seat(&self) -> Option<&(WlSeat, u32)> {
        self.observed_seats.get(self.last_pos)
    }
}
