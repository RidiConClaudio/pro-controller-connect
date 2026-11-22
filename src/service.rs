//! Runs the whole cycle on its own thread: monitors the paired controller, removes a "stuck" pairing,
//! searches for the controller in SYNC mode and pairs it again automatically.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

use serde::Serialize;
#[cfg(windows)]
use windows::{
    core::{Interface, HSTRING},
    Devices::{
        Bluetooth::{BluetoothAdapter, BluetoothConnectionStatus, BluetoothDevice},
        Enumeration::*,
        Radios::{Radio, RadioAccessStatus, RadioState},
    },
    Foundation::{IReference, TypedEventHandler},
};
#[cfg(windows)]
use windows_collections::IIterable;

use crate::{
    loc::{t, tf},
    settings::{self, Settings},
};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Debug)]
pub enum LinkState {
    Starting,
    Connected,
    Disconnected,
    NotPaired,
    Removing,
    Searching,
    Pairing,
    BluetoothOff,
    NoAdapter,
    Error,
}

/// Steps of the re-pairing procedure (shown by the UI stepper).
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Debug)]
#[repr(u8)]
pub enum FlowStep {
    None = 0,
    Remove = 1,
    Sync = 2,
    Pair = 3,
    Done = 4,
}

#[derive(Clone, PartialEq, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub state: LinkState,
    pub step: u8,
    pub title: String,
    pub detail: String,
    pub busy: bool,
    pub auto_flow: bool,
}

pub enum ServiceEvent {
    Status(Status),
    Log(String),
    Notify(String, String),
}

enum Cmd {
    ConnectNow,
    RestartBluetooth,
}

enum FlowError {
    Cancelled,
    Msg(String),
}

#[cfg(windows)]
impl From<windows::core::Error> for FlowError {
    fn from(e: windows::core::Error) -> Self {
        FlowError::Msg(e.message())
    }
}

impl From<String> for FlowError {
    fn from(s: String) -> Self {
        FlowError::Msg(s)
    }
}

type FlowResult<T> = Result<T, FlowError>;

struct State {
    current: Status,
    flow_running: bool,
    flow_is_auto: bool,
    auto_suspended: bool,
    was_connected: bool,
    disconnected_since: Option<Instant>,
    hold_until: Option<Instant>,
}

struct Inner {
    settings: Arc<Mutex<Settings>>,
    sink: Box<dyn Fn(ServiceEvent) + Send + Sync>,
    state: Mutex<State>,
    cmds: Mutex<VecDeque<Cmd>>,
    wake: Condvar,
    cancel: AtomicBool,
    quit: AtomicBool,
}

#[derive(Clone)]
pub struct Service(Arc<Inner>);

#[cfg(windows)]
const AEP_PROPS: [&str; 3] =
    ["System.Devices.Aep.DeviceAddress", "System.Devices.Aep.IsConnected", "System.Devices.Aep.IsPaired"];

#[cfg(windows)]
struct ControllerInfo {
    info: DeviceInformation,
    name: String,
    connected: bool,
}

impl Service {
    pub fn new(settings: Arc<Mutex<Settings>>, sink: impl Fn(ServiceEvent) + Send + Sync + 'static) -> Self {
        let current = Status {
            state: LinkState::Starting,
            step: 0,
            title: t("starting_title"),
            detail: t("starting_detail"),
            busy: true,
            auto_flow: false,
        };
        Service(Arc::new(Inner {
            settings,
            sink: Box::new(sink),
            state: Mutex::new(State {
                current,
                flow_running: false,
                flow_is_auto: false,
                auto_suspended: false,
                was_connected: false,
                disconnected_since: None,
                hold_until: None,
            }),
            cmds: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            cancel: AtomicBool::new(false),
            quit: AtomicBool::new(false),
        }))
    }

    pub fn start(&self) {
        let me = self.clone();
        std::thread::Builder::new()
            .name("controller-service".into())
            .spawn(move || me.monitor_loop())
            .expect("service thread");
    }

    // ───────────────────────────── Public API (any thread) ─────────────────────────────

    pub fn current(&self) -> Status {
        self.0.state.lock().unwrap().current.clone()
    }

    pub fn flow_running(&self) -> bool {
        self.0.state.lock().unwrap().flow_running
    }

    /// Manual procedure: remove, search (90 s) and pair again.
    pub fn connect_now(&self) {
        if self.flow_running() {
            return;
        }
        self.push(Cmd::ConnectNow);
    }

    pub fn cancel_flow(&self) {
        let mut st = self.0.state.lock().unwrap();
        if !st.flow_running {
            return;
        }
        if st.flow_is_auto {
            st.auto_suspended = true; // user stopped it: no immediate restart
        }
        drop(st);
        self.0.cancel.store(true, Ordering::SeqCst);
        self.0.wake.notify_all();
    }

    pub fn on_auto_mode_changed(&self) {
        let auto = self.0.settings.lock().unwrap().auto_mode;
        let mut st = self.0.state.lock().unwrap();
        st.auto_suspended = false;
        st.hold_until = None;
        if !auto && st.flow_running && st.flow_is_auto {
            self.0.cancel.store(true, Ordering::SeqCst);
        }
        drop(st);
        self.0.wake.notify_all();
    }

    pub fn restart_bluetooth(&self) {
        self.cancel_flow();
        self.push(Cmd::RestartBluetooth);
    }

    pub fn stop(&self) {
        self.0.quit.store(true, Ordering::SeqCst);
        self.0.cancel.store(true, Ordering::SeqCst);
        self.0.wake.notify_all();
    }

    fn push(&self, cmd: Cmd) {
        self.0.cmds.lock().unwrap().push_back(cmd);
        self.0.wake.notify_all();
    }

    // ───────────────────────────── Monitoring ─────────────────────────────

    fn monitor_loop(&self) {
        let filter = self.0.settings.lock().unwrap().name_filter.clone();
        self.write_log(&tf("log_started", &[&filter]));
        while !self.0.quit.load(Ordering::SeqCst) {
            loop {
                let cmd = self.0.cmds.lock().unwrap().pop_front();
                match cmd {
                    Some(Cmd::ConnectNow) => {
                        {
                            let mut st = self.0.state.lock().unwrap();
                            st.auto_suspended = false;
                            st.hold_until = None;
                        }
                        self.run_flow(false);
                    }
                    Some(Cmd::RestartBluetooth) => self.do_restart_bluetooth(),
                    None => break,
                }
            }
            if self.0.quit.load(Ordering::SeqCst) {
                break;
            }
            if !self.holding() {
                match self.tick() {
                    Ok(true) => self.run_flow(true),
                    Ok(false) => {}
                    Err(msg) => self.write_log(&tf("log_monitor_error", &[&msg])),
                }
            }
            // every 2 s, or earlier when a command arrives
            let q = self.0.cmds.lock().unwrap();
            if q.is_empty() {
                let _ = self.0.wake.wait_timeout(q, Duration::from_secs(2));
            }
        }
    }

    fn holding(&self) -> bool {
        self.0.state.lock().unwrap().hold_until.is_some_and(|h| Instant::now() < h)
    }

    fn hold(&self, d: Option<Duration>) {
        self.0.state.lock().unwrap().hold_until = d.map(|d| Instant::now() + d);
    }

    #[cfg(windows)]
    fn tick(&self) -> Result<bool, String> {
        self.tick_win().map_err(|e| e.message())
    }

    /// One check of the controller. Returns true when the automatic procedure should start.
    #[cfg(windows)]
    fn tick_win(&self) -> windows::core::Result<bool> {
        let Some(adapter) = BluetoothAdapter::GetDefaultAsync().and_then(|op| op.join()).ok() else {
            self.publish(LinkState::NoAdapter, FlowStep::None, t("no_adapter_title"), t("no_adapter_detail"), false);
            return Ok(false);
        };
        if let Ok(radio) = adapter.GetRadioAsync().and_then(|op| op.join()) {
            if radio.State()? != RadioState::On {
                self.0.state.lock().unwrap().disconnected_since = None;
                self.publish(LinkState::BluetoothOff, FlowStep::None, t("bt_off_title"), t("bt_off_detail"), false);
                return Ok(false);
            }
        }

        let paired = self.paired_controllers()?;
        let (auto_mode, grace) = {
            let s = self.0.settings.lock().unwrap();
            (s.auto_mode, s.grace_seconds)
        };
        let mut st = self.0.state.lock().unwrap();
        let auto = auto_mode && !st.auto_suspended;

        if let Some(c) = paired.iter().find(|c| c.connected) {
            let newly = !st.was_connected;
            st.was_connected = true;
            st.disconnected_since = None;
            st.auto_suspended = false;
            drop(st);
            if newly {
                self.write_log(&tf("log_connected", &[&c.name]));
            }
            self.publish(LinkState::Connected, FlowStep::None, t("connected_title"), tf("connected_detail", &[&c.name]), false);
            return Ok(false);
        }

        if st.was_connected {
            st.was_connected = false;
            drop(st);
            self.write_log(&t("log_disconnected"));
            st = self.0.state.lock().unwrap();
        }

        if !paired.is_empty() {
            let since = *st.disconnected_since.get_or_insert_with(Instant::now);
            let remaining = grace - since.elapsed().as_secs() as i32;
            drop(st);
            if auto && remaining <= 0 {
                self.write_log(&t("log_autorepair"));
                return Ok(true);
            }
            let detail = if auto { tf("disconnected_auto", &[&remaining.max(0)]) } else { t("disconnected_manual") };
            self.publish(LinkState::Disconnected, FlowStep::None, t("disconnected_title"), detail, false);
        } else {
            st.disconnected_since = None;
            let suspended = st.auto_suspended;
            drop(st);
            if auto {
                return Ok(true);
            }
            let detail = if suspended { t("notpaired_paused") } else { t("notpaired_detail") };
            self.publish(LinkState::NotPaired, FlowStep::None, t("notpaired_title"), detail, false);
        }
        Ok(false)
    }

    // ───────────────────────────── Re-pairing procedure ─────────────────────────────

    fn run_flow(&self, is_auto: bool) {
        {
            let mut st = self.0.state.lock().unwrap();
            if st.flow_running {
                return;
            }
            st.flow_running = true;
            st.flow_is_auto = is_auto;
        }
        self.0.cancel.store(false, Ordering::SeqCst);

        match self.flow(is_auto) {
            Ok(()) => {}
            Err(FlowError::Cancelled) => {
                self.write_log(&t("log_cancelled"));
                self.hold(None);
            }
            Err(FlowError::Msg(msg)) => {
                self.write_log(&tf("log_error", &[&msg]));
                self.fail(t("error_title"), msg);
            }
        }

        let status = {
            let mut st = self.0.state.lock().unwrap();
            st.flow_running = false;
            st.flow_is_auto = false;
            st.disconnected_since = None;
            st.current.busy = false;
            st.current.auto_flow = false;
            st.current.clone()
        };
        (self.0.sink)(ServiceEvent::Status(status));
    }

    #[cfg(windows)]
    fn flow(&self, is_auto: bool) -> FlowResult<()> {
        // 1. Remove the existing (stuck) pairing
        self.publish(LinkState::Removing, FlowStep::Remove, t("removing_title"), t("removing_detail"), true);
        let paired = self.paired_controllers()?;
        for c in &paired {
            let r = c.info.Pairing()?.UnpairAsync()?.join()?;
            self.write_log(&tf("log_removed", &[&c.name, &unpair_status_name(r.Status()?)]));
        }
        if !paired.is_empty() {
            self.sleep(Duration::from_millis(1500))?;
        }

        // 2. Wait for the controller in SYNC mode
        let (title, detail, log) = if is_auto {
            ("waiting_title_auto", "waiting_detail_auto", "log_search_auto")
        } else {
            ("waiting_title", "waiting_detail", "log_search")
        };
        self.publish(LinkState::Searching, FlowStep::Sync, t(title), t(detail), true);
        self.write_log(&t(log));

        let timeout = if is_auto { None } else { Some(Duration::from_secs(90)) };
        let Some(device) = self.discover(timeout, is_auto)? else {
            self.write_log(&t("log_notfound"));
            self.fail(t("notfound_title"), t("notfound_detail"));
            return Ok(());
        };
        let name = device.Name()?.to_string();
        self.write_log(&tf("log_found", &[&name]));

        // 3. Pair
        self.publish(LinkState::Pairing, FlowStep::Pair, t("pairing_title"), name.clone(), true);
        if !self.pair(&device)? {
            self.fail(t("pairfail_title"), t("pairfail_detail"));
            return Ok(());
        }

        // 4. Wait for the connection
        self.publish(LinkState::Pairing, FlowStep::Done, t("connecting_title"), t("connecting_detail"), true);
        let ok = self.wait_connected(&device.Id()?, Duration::from_secs(20))?;
        self.0.state.lock().unwrap().was_connected = ok;
        if ok {
            self.write_log(&tf("log_paired_connected", &[&name]));
            self.publish(LinkState::Connected, FlowStep::Done, t("connected_title"), tf("connected_detail", &[&name]), true);
            (self.0.sink)(ServiceEvent::Notify(t("connected_title"), tf("notify_connected_text", &[&name])));
        } else {
            self.write_log(&t("log_paired_not_connected"));
            self.publish(LinkState::Disconnected, FlowStep::Pair, t("paired_title"), t("paired_detail"), true);
        }
        self.hold(Some(Duration::from_secs(4)));
        Ok(())
    }

    fn fail(&self, title: String, detail: String) {
        let step = self.0.state.lock().unwrap().current.step;
        self.publish_step(LinkState::Error, step, title, detail, true);
        self.hold(Some(Duration::from_secs(8)));
    }

    #[cfg(windows)]
    fn do_restart_bluetooth(&self) {
        let result: windows::core::Result<()> = (|| {
            let access = Radio::RequestAccessAsync()?.join()?;
            if access != RadioAccessStatus::Allowed {
                self.write_log(&tf("log_radio_denied", &[&radio_access_name(access)]));
                return Ok(());
            }
            let Some(radio) = get_radio() else {
                self.write_log(&t("log_no_radio"));
                return Ok(());
            };
            self.write_log(&t("log_radio_restart"));
            self.publish(LinkState::Removing, FlowStep::None, t("restarting_title"), t("restarting_detail"), true);
            self.hold(Some(Duration::from_secs(5)));
            radio.SetStateAsync(RadioState::Off)?.join()?;
            std::thread::sleep(Duration::from_millis(2500));
            radio.SetStateAsync(RadioState::On)?.join()?;
            self.write_log(&t("log_radio_on"));
            self.hold(Some(Duration::from_secs(2)));
            Ok(())
        })();
        if let Err(e) = result {
            self.write_log(&tf("log_radio_fail", &[&e.message()]));
        }
    }

    // ───────────────────────────── Bluetooth (WinRT) ─────────────────────────────

    #[cfg(windows)]
    fn paired_controllers(&self) -> windows::core::Result<Vec<ControllerInfo>> {
        let selector = BluetoothDevice::GetDeviceSelectorFromPairingState(true)?;
        let infos = DeviceInformation::FindAllAsyncWithKindAqsFilterAndAdditionalProperties(
            &selector,
            &aep_props(),
            DeviceInformationKind::AssociationEndpoint,
        )?
        .join()?;
        let filter = self.0.settings.lock().unwrap().clone();
        let mut result = Vec::new();
        for i in 0..infos.Size()? {
            let d = infos.GetAt(i)?;
            let name = d.Name()?.to_string();
            if !filter.matches(&name) {
                continue;
            }
            let connected = match is_connected(&d.Id()?) {
                Ok(c) => c,
                Err(_) => d
                    .Properties()
                    .and_then(|p| p.Lookup(&HSTRING::from("System.Devices.Aep.IsConnected")))
                    .and_then(|v| v.cast::<IReference<bool>>())
                    .and_then(|v| v.Value())
                    .unwrap_or(false),
            };
            result.push(ControllerInfo { info: d, name, connected });
        }
        Ok(result)
    }

    /// Searches for an unpaired controller (i.e. in SYNC mode). No timeout searches forever.
    #[cfg(windows)]
    fn discover(&self, timeout: Option<Duration>, duty_cycle: bool) -> FlowResult<Option<DeviceInformation>> {
        let deadline = timeout.map(|t| Instant::now() + t);
        while deadline.is_none_or(|d| Instant::now() < d) {
            self.check_cancel()?;
            let mut slice = Duration::from_secs(20);
            if let Some(d) = deadline {
                slice = slice.min(d.saturating_duration_since(Instant::now()));
            }
            if let Some(found) = self.scan_once(slice)? {
                return Ok(Some(found));
            }
            // In automatic mode the search runs in cycles so the radio isn't busy non-stop.
            if duty_cycle {
                self.sleep(Duration::from_secs(3))?;
            }
        }
        Ok(None)
    }

    #[cfg(windows)]
    fn scan_once(&self, duration: Duration) -> FlowResult<Option<DeviceInformation>> {
        let (tx, rx) = mpsc::channel::<Option<DeviceInformation>>();
        let seen: Arc<Mutex<HashMap<String, DeviceInformation>>> = Arc::default();
        let filter = self.0.settings.lock().unwrap().name_filter.clone();
        let watcher = DeviceInformation::CreateWatcherWithKindAqsFilterAndAdditionalProperties(
            &BluetoothDevice::GetDeviceSelectorFromPairingState(false)?,
            &aep_props(),
            DeviceInformationKind::AssociationEndpoint,
        )?;

        let check = {
            let (tx, filter) = (tx.clone(), filter.clone());
            move |d: &DeviceInformation| {
                let name = d.Name().map(|n| n.to_string()).unwrap_or_default();
                if settings::name_matches(&filter, &name) && d.Pairing().and_then(|p| p.CanPair()).unwrap_or(false) {
                    let _ = tx.send(Some(d.clone()));
                }
            }
        };
        {
            let (seen, check) = (seen.clone(), check.clone());
            watcher.Added(&TypedEventHandler::<DeviceWatcher, DeviceInformation>::new(move |_, d| {
                if let Some(d) = d.as_ref() {
                    seen.lock().unwrap().insert(d.Id()?.to_string(), d.clone());
                    check(d);
                }
                Ok(())
            }))?;
        }
        {
            let seen = seen.clone();
            watcher.Updated(&TypedEventHandler::<DeviceWatcher, DeviceInformationUpdate>::new(move |_, u| {
                let Some(u) = u.as_ref() else { return Ok(()) };
                let d = {
                    let map = seen.lock().unwrap();
                    let Some(d) = map.get(&u.Id()?.to_string()) else { return Ok(()) };
                    d.Update(u)?;
                    d.clone()
                };
                check(&d);
                Ok(())
            }))?;
        }
        watcher.Stopped(&TypedEventHandler::<DeviceWatcher, windows::core::IInspectable>::new(move |_, _| {
            let _ = tx.send(None);
            Ok(())
        }))?;

        watcher.Start()?;
        let end = Instant::now() + duration;
        let result = loop {
            if self.cancelled() {
                break Err(FlowError::Cancelled);
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Ok(None);
            }
            match rx.recv_timeout(left.min(Duration::from_millis(200))) {
                Ok(found) => break Ok(found),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(None),
            }
        };
        if let Ok(s) = watcher.Status() {
            if s == DeviceWatcherStatus::Started || s == DeviceWatcherStatus::EnumerationCompleted {
                let _ = watcher.Stop();
            }
        }
        result
    }

    #[cfg(windows)]
    fn pair(&self, device: &DeviceInformation) -> FlowResult<bool> {
        let kinds = DevicePairingKinds::ConfirmOnly
            | DevicePairingKinds::DisplayPin
            | DevicePairingKinds::ProvidePin
            | DevicePairingKinds::ConfirmPinMatch;

        for attempt in 1..=3 {
            self.check_cancel()?;
            let custom = device.Pairing()?.Custom()?;
            let token = custom.PairingRequested(&TypedEventHandler::<
                DeviceInformationCustomPairing,
                DevicePairingRequestedEventArgs,
            >::new(|_, args| {
                if let Some(args) = args.as_ref() {
                    if args.PairingKind()? == DevicePairingKinds::ProvidePin {
                        args.AcceptWithPin(&HSTRING::from("0000"))?;
                    } else {
                        args.Accept()?;
                    }
                }
                Ok(())
            }))?;
            let result = custom.PairWithProtectionLevelAsync(kinds, DevicePairingProtectionLevel::None).and_then(|op| op.join());
            let _ = custom.RemovePairingRequested(token);
            let status = result?.Status()?;
            self.write_log(&tf("log_pair_attempt", &[&attempt, &pairing_status_name(status)]));
            if status == DevicePairingResultStatus::Paired || status == DevicePairingResultStatus::AlreadyPaired {
                return Ok(true);
            }
            self.sleep(Duration::from_millis(1500))?;
        }
        Ok(false)
    }

    #[cfg(windows)]
    fn wait_connected(&self, id: &HSTRING, timeout: Duration) -> FlowResult<bool> {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if is_connected(id).unwrap_or(false) {
                return Ok(true);
            }
            self.sleep(Duration::from_millis(700))?;
        }
        Ok(false)
    }

    // ───────────────────────────── Bluetooth (Linux / macOS) ─────────────────────────────

    #[cfg(not(windows))]
    fn tick(&self) -> Result<bool, String> {
        match unix_bt::adapter_state()? {
            unix_bt::BtAdapter::NoAdapter => {
                self.publish(LinkState::NoAdapter, FlowStep::None, t("no_adapter_title"), t("no_adapter_detail"), false);
                return Ok(false);
            }
            unix_bt::BtAdapter::Off => {
                self.0.state.lock().unwrap().disconnected_since = None;
                self.publish(LinkState::BluetoothOff, FlowStep::None, t("bt_off_title"), t("bt_off_detail"), false);
                return Ok(false);
            }
            unix_bt::BtAdapter::On => {}
        }

        let (filter, auto_mode, grace) = {
            let s = self.0.settings.lock().unwrap();
            (s.name_filter.clone(), s.auto_mode, s.grace_seconds)
        };
        let paired = unix_bt::paired_controllers(&filter)?;
        let mut st = self.0.state.lock().unwrap();
        let auto = auto_mode && !st.auto_suspended;

        if let Some(c) = paired.iter().find(|c| c.connected) {
            let newly = !st.was_connected;
            st.was_connected = true;
            st.disconnected_since = None;
            st.auto_suspended = false;
            drop(st);
            if newly {
                self.write_log(&tf("log_connected", &[&c.name]));
            }
            self.publish(LinkState::Connected, FlowStep::None, t("connected_title"), tf("connected_detail", &[&c.name]), false);
            return Ok(false);
        }

        if st.was_connected {
            st.was_connected = false;
            drop(st);
            self.write_log(&t("log_disconnected"));
            st = self.0.state.lock().unwrap();
        }

        if !paired.is_empty() {
            let since = *st.disconnected_since.get_or_insert_with(Instant::now);
            let remaining = grace - since.elapsed().as_secs() as i32;
            drop(st);
            if auto && remaining <= 0 {
                self.write_log(&t("log_autorepair"));
                return Ok(true);
            }
            let detail = if auto { tf("disconnected_auto", &[&remaining.max(0)]) } else { t("disconnected_manual") };
            self.publish(LinkState::Disconnected, FlowStep::None, t("disconnected_title"), detail, false);
        } else {
            st.disconnected_since = None;
            let suspended = st.auto_suspended;
            drop(st);
            if auto {
                return Ok(true);
            }
            let detail = if suspended { t("notpaired_paused") } else { t("notpaired_detail") };
            self.publish(LinkState::NotPaired, FlowStep::None, t("notpaired_title"), detail, false);
        }
        Ok(false)
    }

    #[cfg(not(windows))]
    fn flow(&self, is_auto: bool) -> FlowResult<()> {
        let filter = self.0.settings.lock().unwrap().name_filter.clone();

        // 1. Remove the existing (stuck) pairing
        self.publish(LinkState::Removing, FlowStep::Remove, t("removing_title"), t("removing_detail"), true);
        let paired = unix_bt::paired_controllers(&filter)?;
        for c in &paired {
            let status = unix_bt::unpair(&c.id).unwrap_or_else(|e| e);
            self.write_log(&tf("log_removed", &[&c.name, &status]));
        }
        if !paired.is_empty() {
            self.sleep(Duration::from_millis(1500))?;
        }

        // 2. Wait for the controller in SYNC mode
        let (title, detail, log) = if is_auto {
            ("waiting_title_auto", "waiting_detail_auto", "log_search_auto")
        } else {
            ("waiting_title", "waiting_detail", "log_search")
        };
        self.publish(LinkState::Searching, FlowStep::Sync, t(title), t(detail), true);
        self.write_log(&t(log));

        let timeout = if is_auto { None } else { Some(Duration::from_secs(90)) };
        let Some(device) = self.discover_unix(&filter, timeout, is_auto)? else {
            self.write_log(&t("log_notfound"));
            self.fail(t("notfound_title"), t("notfound_detail"));
            return Ok(());
        };
        let name = device.name.clone();
        self.write_log(&tf("log_found", &[&name]));

        // 3. Pair
        self.publish(LinkState::Pairing, FlowStep::Pair, t("pairing_title"), name.clone(), true);
        let mut paired_ok = false;
        for attempt in 1..=3 {
            self.check_cancel()?;
            let (ok, status) = unix_bt::pair_and_connect(&device.id);
            self.write_log(&tf("log_pair_attempt", &[&attempt, &status]));
            if ok {
                paired_ok = true;
                break;
            }
            self.sleep(Duration::from_millis(1500))?;
        }
        if !paired_ok {
            self.fail(t("pairfail_title"), t("pairfail_detail"));
            return Ok(());
        }

        // 4. Wait for the connection
        self.publish(LinkState::Pairing, FlowStep::Done, t("connecting_title"), t("connecting_detail"), true);
        let end = Instant::now() + Duration::from_secs(20);
        let mut ok = false;
        while Instant::now() < end {
            if unix_bt::is_connected(&device.id).unwrap_or(false) {
                ok = true;
                break;
            }
            self.sleep(Duration::from_millis(700))?;
        }
        self.0.state.lock().unwrap().was_connected = ok;
        if ok {
            self.write_log(&tf("log_paired_connected", &[&name]));
            self.publish(LinkState::Connected, FlowStep::Done, t("connected_title"), tf("connected_detail", &[&name]), true);
            (self.0.sink)(ServiceEvent::Notify(t("connected_title"), tf("notify_connected_text", &[&name])));
        } else {
            self.write_log(&t("log_paired_not_connected"));
            self.publish(LinkState::Disconnected, FlowStep::Pair, t("paired_title"), t("paired_detail"), true);
        }
        self.hold(Some(Duration::from_secs(4)));
        Ok(())
    }

    #[cfg(not(windows))]
    fn discover_unix(&self, filter: &str, timeout: Option<Duration>, duty_cycle: bool) -> FlowResult<Option<unix_bt::UnixController>> {
        let deadline = timeout.map(|t| Instant::now() + t);
        while deadline.is_none_or(|d| Instant::now() < d) {
            self.check_cancel()?;
            let mut slice = Duration::from_secs(20);
            if let Some(d) = deadline {
                slice = slice.min(d.saturating_duration_since(Instant::now()));
            }
            if let Some(found) = unix_bt::scan_once(filter, slice, &|| self.cancelled())? {
                return Ok(Some(found));
            }
            if duty_cycle {
                self.sleep(Duration::from_secs(3))?;
            }
        }
        Ok(None)
    }

    #[cfg(not(windows))]
    fn do_restart_bluetooth(&self) {
        self.write_log(&t("log_radio_restart"));
        self.publish(LinkState::Removing, FlowStep::None, t("restarting_title"), t("restarting_detail"), true);
        self.hold(Some(Duration::from_secs(5)));
        match unix_bt::restart_radio() {
            Ok(()) => {
                self.write_log(&t("log_radio_on"));
                self.hold(Some(Duration::from_secs(2)));
            }
            Err(msg) => {
                self.write_log(&tf("log_radio_fail", &[&msg]));
            }
        }
    }

    // ───────────────────────────── Utility ─────────────────────────────

    fn cancelled(&self) -> bool {
        self.0.cancel.load(Ordering::SeqCst) || self.0.quit.load(Ordering::SeqCst)
    }

    fn check_cancel(&self) -> FlowResult<()> {
        if self.cancelled() {
            Err(FlowError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// A pause that ends early when the procedure is cancelled.
    fn sleep(&self, d: Duration) -> FlowResult<()> {
        let end = Instant::now() + d;
        let mut q = self.0.cmds.lock().unwrap();
        loop {
            if self.cancelled() {
                return Err(FlowError::Cancelled);
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            q = self.0.wake.wait_timeout(q, left).unwrap().0;
        }
    }

    fn publish(&self, state: LinkState, step: FlowStep, title: String, detail: String, force: bool) {
        self.publish_step(state, step as u8, title, detail, force);
    }

    fn publish_step(&self, state: LinkState, step: u8, title: String, detail: String, force: bool) {
        let mut st = self.0.state.lock().unwrap();
        let holding = st.hold_until.is_some_and(|h| Instant::now() < h);
        if !force && (st.flow_running || holding) {
            return;
        }
        let status = Status { state, step, title, detail, busy: st.flow_running, auto_flow: st.flow_is_auto };
        if status == st.current {
            return;
        }
        st.current = status.clone();
        drop(st);
        (self.0.sink)(ServiceEvent::Status(status));
    }

    fn write_log(&self, message: &str) {
        (self.0.sink)(ServiceEvent::Log(message.to_string()));
        settings::write_log_file(message);
    }
}

#[cfg(windows)]
fn aep_props() -> IIterable<HSTRING> {
    AEP_PROPS.iter().map(|p| HSTRING::from(*p)).collect::<Vec<_>>().into()
}

#[cfg(windows)]
fn get_radio() -> Option<Radio> {
    BluetoothAdapter::GetDefaultAsync().and_then(|op| op.join()).and_then(|a| a.GetRadioAsync()?.join()).ok()
}

#[cfg(windows)]
fn is_connected(id: &HSTRING) -> windows::core::Result<bool> {
    let bt = BluetoothDevice::FromIdAsync(id)?.join()?;
    let connected = bt.ConnectionStatus()? == BluetoothConnectionStatus::Connected;
    let _ = bt.Close();
    Ok(connected)
}

#[cfg(windows)]
fn pairing_status_name(s: DevicePairingResultStatus) -> &'static str {
    const NAMES: [&str; 20] = [
        "Paired", "NotReadyToPair", "NotPaired", "AlreadyPaired", "ConnectionRejected", "TooManyConnections",
        "HardwareFailure", "AuthenticationTimeout", "AuthenticationNotAllowed", "AuthenticationFailure",
        "NoSupportedProfiles", "ProtectionLevelCouldNotBeMet", "AccessDenied", "InvalidCeremonyData",
        "PairingCanceled", "OperationAlreadyInProgress", "RequiredHandlerNotRegistered", "RejectedByHandler",
        "RemoteDeviceHasAssociation", "Failed",
    ];
    NAMES.get(s.0 as usize).copied().unwrap_or("Failed")
}

#[cfg(windows)]
fn unpair_status_name(s: DeviceUnpairingResultStatus) -> &'static str {
    const NAMES: [&str; 5] = ["Unpaired", "AlreadyUnpaired", "OperationAlreadyInProgress", "AccessDenied", "Failed"];
    NAMES.get(s.0 as usize).copied().unwrap_or("Failed")
}

#[cfg(windows)]
fn radio_access_name(s: RadioAccessStatus) -> &'static str {
    const NAMES: [&str; 4] = ["Unspecified", "Allowed", "DeniedByUser", "DeniedBySystem"];
    NAMES.get(s.0 as usize).copied().unwrap_or("Unspecified")
}

#[cfg(not(windows))]
mod unix_bt {
    use super::{FlowError, FlowResult};
    use crate::settings;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    pub enum BtAdapter {
        On,
        Off,
        NoAdapter,
    }

    #[derive(Clone, Debug)]
    pub struct UnixController {
        pub id: String,
        pub name: String,
        pub connected: bool,
    }

    #[cfg(target_os = "linux")]
    fn bt_cmd() -> Command {
        let mut c = Command::new("bluetoothctl");
        c.env("LC_ALL", "C").env("TERM", "dumb");
        c
    }

    #[cfg(target_os = "linux")]
    pub fn adapter_state() -> Result<BtAdapter, String> {
        let Ok(out) = bt_cmd().arg("show").output() else {
            return Ok(BtAdapter::NoAdapter);
        };
        let txt = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() || txt.contains("No default controller available") {
            return Ok(BtAdapter::NoAdapter);
        }
        if txt.lines().any(|l| l.trim().eq_ignore_ascii_case("Powered: yes")) {
            Ok(BtAdapter::On)
        } else {
            Ok(BtAdapter::Off)
        }
    }

    #[cfg(target_os = "linux")]
    pub fn paired_controllers(filter: &str) -> Result<Vec<UnixController>, String> {
        let out = bt_cmd()
            .args(["devices", "Paired"])
            .output()
            .ok()
            .filter(|o| o.status.success() && !o.stdout.is_empty())
            .or_else(|| bt_cmd().arg("paired-devices").output().ok());
        let Some(out) = out else { return Ok(vec![]) };
        let txt = String::from_utf8_lossy(&out.stdout);
        let mut list = Vec::new();
        for line in txt.lines() {
            let Some((mac, name)) = parse_device_line(line) else { continue };
            if !settings::name_matches(filter, &name) {
                continue;
            }
            let (paired, connected) = device_info(&mac);
            if paired {
                list.push(UnixController { id: mac, name, connected });
            }
        }
        Ok(list)
    }

    #[cfg(target_os = "linux")]
    fn parse_device_line(line: &str) -> Option<(String, String)> {
        let idx = line.find("Device ")?;
        let rest = line[idx + 7..].trim();
        let (mac, name) = rest.split_once(' ')?;
        if mac.len() == 17 && mac.chars().filter(|&c| c == ':').count() == 5 {
            Some((mac.trim().to_string(), name.trim().to_string()))
        } else {
            None
        }
    }

    #[cfg(target_os = "linux")]
    fn device_info(mac: &str) -> (bool, bool) {
        let Ok(out) = bt_cmd().args(["info", mac]).output() else {
            return (false, false);
        };
        let txt = String::from_utf8_lossy(&out.stdout);
        let paired = txt.lines().any(|l| l.trim().eq_ignore_ascii_case("Paired: yes"));
        let connected = txt.lines().any(|l| l.trim().eq_ignore_ascii_case("Connected: yes"));
        (paired, connected)
    }

    #[cfg(target_os = "linux")]
    pub fn unpair(id: &str) -> Result<String, String> {
        let out = bt_cmd().args(["remove", id]).output().map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok("Unpaired".into())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    #[cfg(target_os = "linux")]
    pub fn scan_once(filter: &str, duration: Duration, cancelled: &dyn Fn() -> bool) -> FlowResult<Option<UnixController>> {
        use std::io::Write;
        let mut child = bt_cmd()
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| FlowError::Msg(e.to_string()))?;
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(b"scan on\n");
            let _ = stdin.flush();
        }
        let end = Instant::now() + duration;
        let mut found = None;
        while Instant::now() < end {
            if cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(FlowError::Cancelled);
            }
            if let Ok(out) = bt_cmd().arg("devices").output() {
                let txt = String::from_utf8_lossy(&out.stdout);
                for line in txt.lines() {
                    let Some((mac, name)) = parse_device_line(line) else { continue };
                    if settings::name_matches(filter, &name) {
                        let (paired, _) = device_info(&mac);
                        if !paired {
                            found = Some(UnixController { id: mac, name, connected: false });
                            break;
                        }
                    }
                }
            }
            if found.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(b"scan off\nquit\n");
        }
        let _ = child.kill();
        let _ = child.wait();
        Ok(found)
    }

    #[cfg(target_os = "linux")]
    pub fn pair_and_connect(id: &str) -> (bool, String) {
        use std::io::Write;
        let _ = bt_cmd().args(["trust", id]).output();
        // Register a NoInputNoOutput agent inside an interactive session so pairing works on every
        // desktop environment / window manager (KDE, GNOME, Hyprland, Sway, XFCE) without PIN prompts.
        if let Ok(mut child) = bt_cmd().stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            if let Some(stdin) = child.stdin.as_mut() {
                let cmds = format!("agent NoInputNoOutput\ndefault-agent\ntrust {id}\npair {id}\n");
                let _ = stdin.write_all(cmds.as_bytes());
                let _ = stdin.flush();
            }
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(12) {
                if device_info(id).0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(400));
            }
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(format!("connect {id}\nquit\n").as_bytes());
            }
            std::thread::sleep(Duration::from_millis(800));
            let _ = child.kill();
            let _ = child.wait();
        } else {
            let _ = bt_cmd().args(["--timeout", "15", "pair", id]).output();
        }
        let _ = bt_cmd().args(["connect", id]).output();
        let (paired, connected) = device_info(id);
        if paired || connected {
            (true, "Paired".into())
        } else {
            (false, "Failed".into())
        }
    }

    #[cfg(target_os = "linux")]
    pub fn is_connected(id: &str) -> Result<bool, String> {
        Ok(device_info(id).1)
    }

    #[cfg(target_os = "linux")]
    pub fn restart_radio() -> Result<(), String> {
        for rf in ["rfkill", "/usr/sbin/rfkill", "/sbin/rfkill"] {
            if Command::new(rf).args(["unblock", "bluetooth"]).output().is_ok() {
                break;
            }
        }
        bt_cmd().args(["power", "off"]).output().map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_millis(2500));
        let out = bt_cmd().args(["power", "on"]).output().map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    // ───────────────────────────── macOS (IOBluetooth JXA + blueutil) ─────────────────────────────

    #[cfg(target_os = "macos")]
    fn jxa(script: &str) -> Result<String, String> {
        let out = Command::new("/usr/bin/osascript")
            .args(["-l", "JavaScript", "-e", script])
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    #[cfg(target_os = "macos")]
    fn blueutil_bin() -> Option<&'static str> {
        for p in ["/opt/homebrew/bin/blueutil", "/usr/local/bin/blueutil", "blueutil"] {
            if Command::new(p).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok() {
                return Some(p);
            }
        }
        None
    }

    #[cfg(target_os = "macos")]
    pub fn adapter_state() -> Result<BtAdapter, String> {
        if let Some(bu) = blueutil_bin() {
            if let Ok(out) = Command::new(bu).arg("-p").output() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if s == "1" {
                    return Ok(BtAdapter::On);
                } else if s == "0" {
                    return Ok(BtAdapter::Off);
                }
            }
        }
        let s = jxa(
            "ObjC.import('IOBluetooth');\
             const c = $.IOBluetoothHostController.defaultController;\
             c.isNil() ? 'none' : (c.powerState ? 'on' : 'off');",
        )
        .unwrap_or_else(|_| "none".into());
        match s.as_str() {
            "on" => Ok(BtAdapter::On),
            "off" => Ok(BtAdapter::Off),
            _ => Ok(BtAdapter::NoAdapter),
        }
    }

    #[cfg(target_os = "macos")]
    pub fn paired_controllers(filter: &str) -> Result<Vec<UnixController>, String> {
        let raw = jxa(
            "ObjC.import('IOBluetooth');\
             const devs = $.IOBluetoothDevice.pairedDevices;\
             const out = [];\
             if (!devs.isNil()) {\
               const n = devs.count;\
               for (let i = 0; i < n; i++) {\
                 const d = devs.objectAtIndex(i);\
                 out.push({\
                   id: ObjC.unwrap(d.addressString) || '',\
                   name: ObjC.unwrap(d.name) || '',\
                   connected: !!d.isConnected\
                 });\
               }\
             }\
             JSON.stringify(out);",
        )?;
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap_or_default();
        let mut list = Vec::new();
        for v in parsed {
            let id = v["id"].as_str().unwrap_or("").to_string();
            let name = v["name"].as_str().unwrap_or("").to_string();
            let connected = v["connected"].as_bool().unwrap_or(false);
            if !id.is_empty() && settings::name_matches(filter, &name) {
                list.push(UnixController { id, name, connected });
            }
        }
        Ok(list)
    }

    #[cfg(target_os = "macos")]
    pub fn unpair(id: &str) -> Result<String, String> {
        if let Some(bu) = blueutil_bin() {
            if let Ok(out) = Command::new(bu).args(["--unpair", id]).output() {
                if out.status.success() {
                    return Ok("Unpaired".into());
                }
            }
        }
        let script = format!(
            "ObjC.import('IOBluetooth');\
             const d = $.IOBluetoothDevice.deviceWithAddressString({:?});\
             if (!d.isNil()) {{\
               d.closeConnection();\
               if (d.respondsToSelector($('remove'))) d.performSelector($('remove'));\
               'Unpaired';\
             }} else {{ 'AlreadyUnpaired'; }}",
            id
        );
        jxa(&script)
    }

    #[cfg(target_os = "macos")]
    pub fn scan_once(filter: &str, duration: Duration, cancelled: &dyn Fn() -> bool) -> FlowResult<Option<UnixController>> {
        let end = Instant::now() + duration;
        while Instant::now() < end {
            if cancelled() {
                return Err(FlowError::Cancelled);
            }
            if let Some(bu) = blueutil_bin() {
                if let Ok(out) = Command::new(bu).args(["--inquiry", "4", "--format", "json"]).output() {
                    let arr: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
                    for v in arr {
                        let name = v["name"].as_str().unwrap_or("");
                        let addr = v["address"].as_str().unwrap_or("");
                        if !addr.is_empty() && settings::name_matches(filter, name) {
                            return Ok(Some(UnixController { id: addr.to_string(), name: name.to_string(), connected: false }));
                        }
                    }
                    continue;
                }
            }
            let raw = jxa(
                "ObjC.import('IOBluetooth');\
                 ObjC.import('Foundation');\
                 const inq = $.IOBluetoothDeviceInquiry.inquiryWithDelegate(null);\
                 inq.inquiryLength = 4;\
                 inq.start();\
                 $.NSRunLoop.currentRunLoop.runUntilDate($.NSDate.dateWithTimeIntervalSinceNow(4.2));\
                 inq.stop();\
                 const devs = inq.foundDevices;\
                 const out = [];\
                 if (!devs.isNil()) {\
                   for (let i = 0; i < devs.count; i++) {\
                     const d = devs.objectAtIndex(i);\
                     out.push({ id: ObjC.unwrap(d.addressString) || '', name: ObjC.unwrap(d.name) || '' });\
                   }\
                 }\
                 JSON.stringify(out);",
            )
            .unwrap_or_default();
            let arr: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap_or_default();
            for v in arr {
                let name = v["name"].as_str().unwrap_or("");
                let addr = v["id"].as_str().unwrap_or("");
                if !addr.is_empty() && settings::name_matches(filter, name) {
                    return Ok(Some(UnixController { id: addr.to_string(), name: name.to_string(), connected: false }));
                }
            }
        }
        Ok(None)
    }

    #[cfg(target_os = "macos")]
    pub fn pair_and_connect(id: &str) -> (bool, String) {
        if let Some(bu) = blueutil_bin() {
            let p = Command::new(bu).args(["--pair", id]).output();
            let _ = Command::new(bu).args(["--connect", id]).output();
            if p.map(|o| o.status.success()).unwrap_or(false) || is_connected(id).unwrap_or(false) {
                return (true, "Paired".into());
            }
        }
        let script = format!(
            "ObjC.import('IOBluetooth');\
             ObjC.import('Foundation');\
             const d = $.IOBluetoothDevice.deviceWithAddressString({:?});\
             if (d.isNil()) {{ 'NotFound'; }} else {{\
               const p = $.IOBluetoothDevicePair.pairWithDevice(d);\
               if (!p.isNil()) p.start();\
               $.NSRunLoop.currentRunLoop.runUntilDate($.NSDate.dateWithTimeIntervalSinceNow(3.0));\
               d.openConnection();\
               (d.isPaired || d.isConnected) ? 'Paired' : 'Failed';\
             }}",
            id
        );
        match jxa(&script) {
            Ok(s) if s == "Paired" => (true, s),
            Ok(s) => (false, s),
            Err(e) => (false, e),
        }
    }

    #[cfg(target_os = "macos")]
    pub fn is_connected(id: &str) -> Result<bool, String> {
        let script = format!(
            "ObjC.import('IOBluetooth');\
             const d = $.IOBluetoothDevice.deviceWithAddressString({:?});\
             (!d.isNil() && !!d.isConnected) ? '1' : '0';",
            id
        );
        Ok(jxa(&script).unwrap_or_default() == "1")
    }

    #[cfg(target_os = "macos")]
    pub fn restart_radio() -> Result<(), String> {
        if let Some(bu) = blueutil_bin() {
            Command::new(bu).args(["-p", "0"]).output().map_err(|e| e.to_string())?;
            std::thread::sleep(Duration::from_millis(2500));
            let out = Command::new(bu).args(["-p", "1"]).output().map_err(|e| e.to_string())?;
            if out.status.success() {
                return Ok(());
            }
        }
        Err("Install 'blueutil' (brew install blueutil) or toggle Bluetooth in System Settings".into())
    }
}
