//! Wire nodes: one PipeWire filter per wire, between the node it comes from
//! and its output, playing each channel pair at its gain; and meter nodes,
//! one per output the wires play on, measuring what is summed there.
//!
//! A wire node has an input and an output port per pair, linked to the two
//! ends by the reconciler; what several wires bring into one channel of an
//! output is summed by PipeWire. It runs in the same graph cycle as its
//! ends, so it adds no latency. A meter node only has inputs, linked to the
//! output's monitor ports, and is passive: it doesn't keep the output
//! running. pipewire-rs has no binding for `pw_filter`, so this calls the C
//! API.

use std::ffi::{CString, c_void};
use std::ptr;
use std::sync::Arc;
use std::time::Instant;

use orpheus_engine::gain::{Ramp, SharedGain};
use orpheus_engine::level::{Level, Meter, SharedLevel};
use pipewire::{self as pw, properties::properties, spa};

use pw::sys as pw_sys;
use spa::sys as spa_sys;

/// A filter node whose realtime thread works on `D`; dropping it removes
/// the node and its links.
struct Filter<D> {
    raw: *mut pw_sys::pw_filter,
    /// Where PipeWire keeps the listener; must outlive the filter.
    _hook: Box<spa_sys::spa_hook>,
    /// Nothing else touches it while the filter is connected.
    dsp: *mut D,
}

impl<D> Filter<D> {
    fn new(
        core: &pw::core::CoreRc,
        name: &str,
        description: &str,
        passive: bool,
        dsp: D,
        events: &'static pw_sys::pw_filter_events,
    ) -> Result<Self, String> {
        let props = properties! {
            *pw::keys::NODE_NAME => name,
            *pw::keys::NODE_DESCRIPTION => description,
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Filter",
            *pw::keys::MEDIA_ROLE => "DSP",
            "node.passive" => if passive { "true" } else { "false" },
            "object.linger" => "false",
        };
        let c_name = CString::new(name).map_err(|e| e.to_string())?;
        // SAFETY: the core is alive; the filter takes the properties.
        let raw =
            unsafe { pw_sys::pw_filter_new(core.as_raw_ptr(), c_name.as_ptr(), props.into_raw()) };
        if raw.is_null() {
            return Err("cannot create a filter".into());
        }
        let dsp = Box::into_raw(Box::new(dsp));
        // SAFETY: zeroed is how C initializes a hook. The filter, the hook
        // and the data stay until `drop`, which destroys the filter first.
        let mut hook: Box<spa_sys::spa_hook> = Box::new(unsafe { std::mem::zeroed() });
        unsafe { pw_sys::pw_filter_add_listener(raw, &mut *hook, events, dsp.cast()) };
        Ok(Self {
            raw,
            _hook: hook,
            dsp,
        })
    }

    /// Adds port `i`; before [`connect`](Self::connect).
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
                self.raw,
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

    /// The realtime thread's data, before [`connect`](Self::connect).
    fn dsp_mut(&mut self) -> &mut D {
        // SAFETY: not connected yet, so the realtime thread can't see it.
        unsafe { &mut *self.dsp }
    }

    fn connect(&self) -> Result<(), String> {
        // SAFETY: the filter is alive.
        let res = unsafe {
            pw_sys::pw_filter_connect(
                self.raw,
                pw_sys::pw_filter_flags_PW_FILTER_FLAG_RT_PROCESS,
                ptr::null_mut(),
                0,
            )
        };
        if res < 0 {
            return Err(format!("connecting the filter: error {res}"));
        }
        Ok(())
    }
}

impl<D> Drop for Filter<D> {
    fn drop(&mut self) {
        // SAFETY: destroying the filter takes it out of the realtime thread
        // and removes the listener; only then the data goes.
        unsafe {
            pw_sys::pw_filter_destroy(self.raw);
            drop(Box::from_raw(self.dsp));
        }
    }
}

const fn events(
    process: unsafe extern "C" fn(*mut c_void, *mut spa_sys::spa_io_position),
) -> pw_sys::pw_filter_events {
    pw_sys::pw_filter_events {
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
    }
}

/// What a wire node's realtime thread works on.
struct WireDsp {
    /// Input and output port of each pair.
    ports: Vec<(*mut c_void, *mut c_void)>,
    gains: Arc<[SharedGain]>,
    ramps: Vec<Ramp>,
    levels: Arc<[SharedLevel]>,
}

/// A wire's filter node. Lives on the PipeWire thread.
pub(crate) struct WireNode {
    _filter: Filter<WireDsp>,
    gains: Arc<[SharedGain]>,
    levels: Arc<[SharedLevel]>,
    /// How loud each pair plays, after its gain.
    meters: Vec<Meter>,
    /// Its machine name.
    pub name: String,
    /// The channel pairs it plays, input then output channel.
    pub pairs: Vec<(usize, usize)>,
}

static WIRE_EVENTS: pw_sys::pw_filter_events = events(process_wire);

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
        let levels: Arc<[SharedLevel]> = pairs.iter().map(|_| SharedLevel::new()).collect();
        let dsp = WireDsp {
            ports: Vec::with_capacity(pairs.len()),
            ramps: gains.iter().map(|g| Ramp::new(g.get())).collect(),
            gains: gains.clone(),
            levels: levels.clone(),
        };
        let mut filter = Filter::new(core, name, description, false, dsp, &WIRE_EVENTS)?;
        for i in 0..pairs.len() {
            let input = filter.add_port(spa_sys::SPA_DIRECTION_INPUT, i)?;
            let output = filter.add_port(spa_sys::SPA_DIRECTION_OUTPUT, i)?;
            filter.dsp_mut().ports.push((input, output));
        }
        filter.connect()?;
        Ok(Self {
            _filter: filter,
            gains,
            meters: vec![Meter::new(); pairs.len()],
            levels,
            name: name.to_owned(),
            pairs,
        })
    }

    /// Plays each pair at the factor in `gains`, ramping to it.
    pub fn set_gains(&self, gains: &[f32]) {
        for (g, &factor) in self.gains.iter().zip(gains) {
            g.set(factor);
        }
    }

    /// Takes the samples played since the last time into the meters; once
    /// per [`WINDOW`](orpheus_engine::level::WINDOW).
    pub fn measure(&mut self) {
        for (meter, level) in self.meters.iter_mut().zip(self.levels.iter()) {
            meter.push(level.take());
        }
    }

    /// How loud each pair plays.
    pub fn levels(&self) -> Vec<Level> {
        self.meters.iter().map(Meter::level).collect()
    }
}

/// A wire node's process callback, on the realtime thread.
unsafe extern "C" fn process_wire(data: *mut c_void, position: *mut spa_sys::spa_io_position) {
    if data.is_null() || position.is_null() {
        return;
    }
    // SAFETY: `data` is the node's `WireDsp`, which only this thread
    // touches while the filter is connected; the position is valid for the
    // cycle.
    let (dsp, frames) = unsafe {
        (
            &mut *data.cast::<WireDsp>(),
            (*position).clock.duration as u32,
        )
    };
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
            dsp.levels[i].record(out);
        }
    }
}

/// What a meter node's realtime thread works on.
struct MeterDsp {
    ports: Vec<*mut c_void>,
    levels: Arc<[SharedLevel]>,
}

/// An output's meter node. Lives on the PipeWire thread.
pub(crate) struct MeterNode {
    _filter: Filter<MeterDsp>,
    levels: Arc<[SharedLevel]>,
    meters: Vec<Meter>,
    /// When a channel last reached full scale.
    pub clipped: Option<Instant>,
    /// Its machine name.
    pub name: String,
    /// How many channels it measures.
    pub channels: usize,
}

static METER_EVENTS: pw_sys::pw_filter_events = events(process_meter);

impl MeterNode {
    /// Creates the passive filter node `name` with an input per channel.
    pub fn new(
        core: &pw::core::CoreRc,
        name: &str,
        description: &str,
        channels: usize,
    ) -> Result<Self, String> {
        let levels: Arc<[SharedLevel]> = (0..channels).map(|_| SharedLevel::new()).collect();
        let dsp = MeterDsp {
            ports: Vec::with_capacity(channels),
            levels: levels.clone(),
        };
        let mut filter = Filter::new(core, name, description, true, dsp, &METER_EVENTS)?;
        for i in 0..channels {
            let input = filter.add_port(spa_sys::SPA_DIRECTION_INPUT, i)?;
            filter.dsp_mut().ports.push(input);
        }
        filter.connect()?;
        Ok(Self {
            _filter: filter,
            levels,
            meters: vec![Meter::new(); channels],
            clipped: None,
            name: name.to_owned(),
            channels,
        })
    }

    /// As [`WireNode::measure`], noting when a channel reached full scale.
    pub fn measure(&mut self) {
        for (meter, level) in self.meters.iter_mut().zip(self.levels.iter()) {
            let window = level.take();
            if window.over > 0 {
                self.clipped = Some(Instant::now());
            }
            meter.push(window);
        }
    }

    /// How loud each channel plays.
    pub fn levels(&self) -> Vec<Level> {
        self.meters.iter().map(Meter::level).collect()
    }
}

/// A meter node's process callback, on the realtime thread.
unsafe extern "C" fn process_meter(data: *mut c_void, position: *mut spa_sys::spa_io_position) {
    if data.is_null() || position.is_null() {
        return;
    }
    // SAFETY: as in `process_wire`.
    let (dsp, frames) = unsafe { (&*data.cast::<MeterDsp>(), (*position).clock.duration as u32) };
    for (i, &input) in dsp.ports.iter().enumerate() {
        // SAFETY: as in `process_wire`.
        unsafe {
            let src = pw_sys::pw_filter_get_dsp_buffer(input, frames).cast::<f32>();
            if !src.is_null() {
                dsp.levels[i].record(std::slice::from_raw_parts(src, frames as usize));
            }
        }
    }
}
