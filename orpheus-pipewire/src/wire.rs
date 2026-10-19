//! Wire nodes: one PipeWire filter per wire, between the node it comes from
//! and its output, playing each channel pair at its gain.
//!
//! The filter has an input and an output port per pair, linked to the two
//! ends by the reconciler; what several wires bring into one channel of an
//! output is summed by PipeWire. It runs in the same graph cycle as its
//! ends, so it adds no latency. pipewire-rs has no binding for `pw_filter`,
//! so this calls the C API.

use std::ffi::{CString, c_void};
use std::ptr;
use std::sync::Arc;

use orpheus_engine::gain::{Ramp, SharedGain};
use pipewire::{self as pw, properties::properties, spa};

use pw::sys as pw_sys;
use spa::sys as spa_sys;

/// What the realtime thread works on; nothing else touches it while the
/// filter is connected.
struct Dsp {
    /// Input and output port of each pair.
    ports: Vec<(*mut c_void, *mut c_void)>,
    gains: Arc<[SharedGain]>,
    ramps: Vec<Ramp>,
}

/// A wire's filter node. Lives on the PipeWire thread; dropping it removes
/// the node and its links.
pub(crate) struct WireNode {
    filter: *mut pw_sys::pw_filter,
    /// Where PipeWire keeps the listener; must outlive the filter.
    _hook: Box<spa_sys::spa_hook>,
    dsp: *mut Dsp,
    gains: Arc<[SharedGain]>,
    /// Its machine name.
    pub name: String,
    /// The channel pairs it plays, input then output channel.
    pub pairs: Vec<(usize, usize)>,
}

static EVENTS: pw_sys::pw_filter_events = pw_sys::pw_filter_events {
    version: pw_sys::PW_VERSION_FILTER_EVENTS,
    destroy: None,
    state_changed: None,
    io_changed: None,
    param_changed: None,
    add_buffer: None,
    remove_buffer: None,
    process: Some(process),
    drained: None,
    command: None,
};

impl WireNode {
    /// Creates the filter node `name` with a port pair per channel pair,
    /// each played at the factor in `gains`.
    pub fn new(
        core: &pw::core::CoreRc,
        name: &str,
        description: &str,
        pairs: Vec<(usize, usize)>,
        gains: &[f32],
    ) -> Result<Self, String> {
        let gains: Arc<[SharedGain]> = gains.iter().map(|&g| SharedGain::new(g)).collect();
        let props = properties! {
            *pw::keys::NODE_NAME => name,
            *pw::keys::NODE_DESCRIPTION => description,
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Filter",
            *pw::keys::MEDIA_ROLE => "DSP",
            "object.linger" => "false",
        };
        let c_name = CString::new(name).map_err(|e| e.to_string())?;
        // SAFETY: the core is alive; the filter takes the properties.
        let filter =
            unsafe { pw_sys::pw_filter_new(core.as_raw_ptr(), c_name.as_ptr(), props.into_raw()) };
        if filter.is_null() {
            return Err("cannot create a filter".into());
        }
        let dsp = Box::into_raw(Box::new(Dsp {
            ports: Vec::with_capacity(pairs.len()),
            ramps: gains.iter().map(|g| Ramp::new(g.get())).collect(),
            gains: gains.clone(),
        }));
        // SAFETY: zeroed is how C initializes a hook. The filter, the hook
        // and the data stay until `drop`, which destroys the filter first.
        let mut hook: Box<spa_sys::spa_hook> = Box::new(unsafe { std::mem::zeroed() });
        unsafe { pw_sys::pw_filter_add_listener(filter, &mut *hook, &EVENTS, dsp.cast()) };
        let mut node = Self {
            filter,
            _hook: hook,
            dsp,
            gains,
            name: name.to_owned(),
            pairs,
        };
        for i in 0..node.pairs.len() {
            let input = node.add_port(spa_sys::SPA_DIRECTION_INPUT, i)?;
            let output = node.add_port(spa_sys::SPA_DIRECTION_OUTPUT, i)?;
            // SAFETY: not connected yet, so the realtime thread can't see it.
            unsafe { (*node.dsp).ports.push((input, output)) };
        }
        // SAFETY: the filter is alive.
        let res = unsafe {
            pw_sys::pw_filter_connect(
                filter,
                pw_sys::pw_filter_flags_PW_FILTER_FLAG_RT_PROCESS,
                ptr::null_mut(),
                0,
            )
        };
        if res < 0 {
            return Err(format!("connecting the filter: error {res}"));
        }
        Ok(node)
    }

    fn add_port(
        &mut self,
        direction: spa_sys::spa_direction,
        i: usize,
    ) -> Result<*mut c_void, String> {
        let input = direction == spa_sys::SPA_DIRECTION_INPUT;
        let name = format!("{}_{}", if input { "in" } else { "out" }, i + 1);
        // The graph knows audio ports by their channel.
        let channel = format!("AUX{i}");
        let props = properties! {
            *pw::keys::FORMAT_DSP => "32 bit float mono audio",
            *pw::keys::PORT_NAME => name.as_str(),
            *pw::keys::AUDIO_CHANNEL => channel.as_str(),
        };
        // SAFETY: the filter is alive and takes the properties; the port
        // data PipeWire allocates is only used as the port's handle.
        let port = unsafe {
            pw_sys::pw_filter_add_port(
                self.filter,
                direction,
                pw_sys::pw_filter_port_flags_PW_FILTER_PORT_FLAG_MAP_BUFFERS,
                size_of::<u64>(),
                props.into_raw(),
                ptr::null_mut(),
                0,
            )
        };
        if port.is_null() {
            return Err(format!("cannot add port {name}"));
        }
        Ok(port)
    }

    /// Plays each pair at the factor in `gains`, ramping to it.
    pub fn set_gains(&self, gains: &[f32]) {
        for (g, &factor) in self.gains.iter().zip(gains) {
            g.set(factor);
        }
    }
}

impl Drop for WireNode {
    fn drop(&mut self) {
        // SAFETY: destroying the filter takes it out of the realtime thread
        // and removes the listener; only then the data goes.
        unsafe {
            pw_sys::pw_filter_destroy(self.filter);
            drop(Box::from_raw(self.dsp));
        }
    }
}

/// The filter's process callback, on the realtime thread.
unsafe extern "C" fn process(data: *mut c_void, position: *mut spa_sys::spa_io_position) {
    if data.is_null() || position.is_null() {
        return;
    }
    // SAFETY: `data` is the node's `Dsp`, which only this thread touches
    // while the filter is connected; the position is valid for the cycle.
    let (dsp, frames) = unsafe { (&mut *data.cast::<Dsp>(), (*position).clock.duration as u32) };
    for (i, &(input, output)) in dsp.ports.iter().enumerate() {
        // SAFETY: the ports belong to this filter; PipeWire's DSP buffers
        // hold `frames` floats for this cycle.
        unsafe {
            let out = pw_sys::pw_filter_get_dsp_buffer(output, frames).cast::<f32>();
            if out.is_null() {
                continue;
            }
            let out = std::slice::from_raw_parts_mut(out, frames as usize);
            let src = pw_sys::pw_filter_get_dsp_buffer(input, frames).cast::<f32>();
            let src = (!src.is_null()).then(|| std::slice::from_raw_parts(src, frames as usize));
            dsp.ramps[i].process(dsp.gains[i].get(), src, out);
        }
    }
}
