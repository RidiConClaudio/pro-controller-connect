//! Reads controller input in real time through Windows.Gaming.Input.

use serde::Serialize;
use windows::{
    Foundation::EventHandler,
    Gaming::Input::{GameControllerSwitchPosition, RawGameController},
};

use crate::loc::{t, tf};

const NINTENDO_VID: u16 = 0x057E;

/// One reading, as sent to the page: buttons, hat switch, axes (0..1).
#[derive(Serialize, PartialEq, Clone)]
pub struct Reading {
    pub b: Vec<bool>,
    pub h: i32,
    pub a: Vec<f64>,
}

pub struct Input {
    controller: Option<RawGameController>,
    buttons: Vec<bool>,
    switches: Vec<GameControllerSwitchPosition>,
    axes: Vec<f64>,
    last: Option<Reading>,
}

impl Input {
    /// `on_change` is called (from any thread) when a controller appears or disappears.
    pub fn new(on_change: impl Fn() + Send + Sync + Clone + 'static) -> Self {
        let added = on_change.clone();
        let _ = RawGameController::RawGameControllerAdded(&EventHandler::<RawGameController>::new(move |_, _| {
            added();
            Ok(())
        }));
        let _ = RawGameController::RawGameControllerRemoved(&EventHandler::<RawGameController>::new(move |_, _| {
            on_change();
            Ok(())
        }));
        let mut me = Input { controller: None, buttons: vec![], switches: vec![], axes: vec![], last: None };
        me.refresh();
        me
    }

    /// Picks the controller to show. Returns true when it changed.
    pub fn refresh(&mut self) -> bool {
        let all: Vec<RawGameController> = RawGameController::RawGameControllers()
            .map(|v| (0..v.Size().unwrap_or(0)).filter_map(|i| v.GetAt(i).ok()).collect())
            .unwrap_or_default();
        let c = all
            .iter()
            .find(|c| c.HardwareVendorId().ok() == Some(NINTENDO_VID))
            .or_else(|| all.iter().find(|c| c.IsWireless().unwrap_or(false)))
            .cloned();
        if c == self.controller {
            return false;
        }
        if let Some(c) = &c {
            self.buttons = vec![false; c.ButtonCount().unwrap_or(0).max(0) as usize];
            self.switches = vec![GameControllerSwitchPosition::Center; c.SwitchCount().unwrap_or(0).max(0) as usize];
            self.axes = vec![0.0; c.AxisCount().unwrap_or(0).max(0) as usize];
        }
        self.controller = c;
        self.last = None;
        true
    }

    pub fn present(&self) -> bool {
        self.controller.is_some()
    }

    /// The current reading, only when it differs from the last one returned.
    pub fn read_changed(&mut self) -> Option<Reading> {
        let c = self.controller.as_ref()?;
        c.GetCurrentReading(&mut self.buttons, &mut self.switches, &mut self.axes).ok()?;
        let r = Reading {
            b: self.buttons.clone(),
            h: self.switches.first().map(|s| s.0).unwrap_or(0),
            a: self.axes.clone(),
        };
        if self.last.as_ref() == Some(&r) {
            return None;
        }
        self.last = Some(r.clone());
        Some(r)
    }

    pub fn forget_last(&mut self) {
        self.last = None;
    }

    pub fn describe(&self) -> Option<String> {
        let c = self.controller.as_ref()?;
        let mut battery = String::new();
        if let Ok(report) = c.TryGetBatteryReport() {
            let rem = report.RemainingCapacityInMilliwattHours().and_then(|v| v.Value());
            let full = report.FullChargeCapacityInMilliwattHours().and_then(|v| v.Value());
            if let (Ok(rem), Ok(full)) = (rem, full) {
                if full > 0 {
                    battery = format!(" · {}", tf("battery", &[&(rem * 100 / full)]));
                }
            }
        }
        let wireless = t(if c.IsWireless().unwrap_or(false) { "wireless" } else { "wired" });
        Some(format!(
            "ID {:04X}:{:04X} · {wireless}{battery}",
            c.HardwareVendorId().unwrap_or(0),
            c.HardwareProductId().unwrap_or(0)
        ))
    }
}
