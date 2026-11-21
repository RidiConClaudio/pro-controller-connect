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
use windows::{
    core::{Interface, HSTRING},
    Devices::{
        Bluetooth::{BluetoothAdapter, BluetoothConnectionStatus, BluetoothDevice},
        Enumeration::*,
        Radios::{Radio, RadioAccessStatus, RadioState},
    },
    Foundation::{IReference, TypedEventHandler},
};
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
    Win(windows::core::Error),
}

impl From<windows::core::Error> for FlowError {
    fn from(e: windows::core::Error) -> Self {
        FlowError::Win(e)
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

const AEP_PROPS: [&str; 3] =
    ["System.Devices.Aep.DeviceAddress", "System.Devices.Aep.IsConnected", "System.Devices.Aep.IsPaired"];

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
                    Err(e) => self.write_log(&tf("log_monitor_error", &[&e.message()])),
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

    /// One check of the controller. Returns true when the automatic procedure should start.
    fn tick(&self) -> windows::core::Result<bool> {
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
            Err(FlowError::Win(e)) => {
                let msg = e.message();
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

fn aep_props() -> IIterable<HSTRING> {
    AEP_PROPS.iter().map(|p| HSTRING::from(*p)).collect::<Vec<_>>().into()
}

fn get_radio() -> Option<Radio> {
    BluetoothAdapter::GetDefaultAsync().and_then(|op| op.join()).and_then(|a| a.GetRadioAsync()?.join()).ok()
}

fn is_connected(id: &HSTRING) -> windows::core::Result<bool> {
    let bt = BluetoothDevice::FromIdAsync(id)?.join()?;
    let connected = bt.ConnectionStatus()? == BluetoothConnectionStatus::Connected;
    let _ = bt.Close();
    Ok(connected)
}

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

fn unpair_status_name(s: DeviceUnpairingResultStatus) -> &'static str {
    const NAMES: [&str; 5] = ["Unpaired", "AlreadyUnpaired", "OperationAlreadyInProgress", "AccessDenied", "Failed"];
    NAMES.get(s.0 as usize).copied().unwrap_or("Failed")
}

fn radio_access_name(s: RadioAccessStatus) -> &'static str {
    const NAMES: [&str; 4] = ["Unspecified", "Allowed", "DeniedByUser", "DeniedBySystem"];
    NAMES.get(s.0 as usize).copied().unwrap_or("Unspecified")
}
