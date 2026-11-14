using System;
using System.Linq;
using Windows.Gaming.Input;

namespace ProControllerConnect;

/// <summary>Reads controller input in real time through Windows.Gaming.Input.</summary>
public sealed class InputMonitor
{
    const ushort NintendoVid = 0x057E;

    public RawGameController? Controller { get; private set; }

    /// <summary>Raised (from any thread) when the controller appears or disappears.</summary>
    public event Action? ControllerChanged;

    public bool[] Buttons { get; private set; } = Array.Empty<bool>();
    public GameControllerSwitchPosition[] Switches { get; private set; } = Array.Empty<GameControllerSwitchPosition>();
    public double[] Axes { get; private set; } = Array.Empty<double>();

    public InputMonitor()
    {
        RawGameController.RawGameControllerAdded += (_, _) => Refresh();
        RawGameController.RawGameControllerRemoved += (_, _) => Refresh();
        Refresh();
    }

    void Refresh()
    {
        var c = RawGameController.RawGameControllers.FirstOrDefault(x => x.HardwareVendorId == NintendoVid)
                ?? RawGameController.RawGameControllers.FirstOrDefault(x => x.IsWireless);
        if (ReferenceEquals(c, Controller)) return;
        Controller = c;
        if (c != null)
        {
            Buttons = new bool[c.ButtonCount];
            Switches = new GameControllerSwitchPosition[c.SwitchCount];
            Axes = new double[c.AxisCount];
        }
        ControllerChanged?.Invoke();
    }

    /// <summary>Updates Buttons/Switches/Axes. Returns false when no controller is present.</summary>
    public bool Read()
    {
        var c = Controller;
        if (c == null) return false;
        try
        {
            c.GetCurrentReading(Buttons, Switches, Axes);
            return true;
        }
        catch
        {
            return false;
        }
    }

    public string Describe()
    {
        var c = Controller;
        if (c == null) return "";
        string battery = "";
        try
        {
            var report = c.TryGetBatteryReport();
            if (report?.RemainingCapacityInMilliwattHours is int rem && report.FullChargeCapacityInMilliwattHours is int full && full > 0)
                battery = " · " + Loc.T("battery", rem * 100 / full);
        }
        catch { }
        return $"ID {c.HardwareVendorId:X4}:{c.HardwareProductId:X4} · {Loc.T(c.IsWireless ? "wireless" : "wired")}{battery}";
    }
}
