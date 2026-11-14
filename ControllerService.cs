using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Windows.Devices.Bluetooth;
using Windows.Devices.Enumeration;
using Windows.Devices.Radios;
using Windows.Foundation;

namespace ProControllerConnect;

public enum LinkState { Starting, Connected, Disconnected, NotPaired, Removing, Searching, Pairing, BluetoothOff, NoAdapter, Error }

/// <summary>Steps of the re-pairing procedure (shown by the UI stepper).</summary>
public enum FlowStep { None = 0, Remove = 1, Sync = 2, Pair = 3, Done = 4 }

public sealed record ServiceStatus(LinkState State, FlowStep Step, string Title, string Detail, bool Busy, bool AutoFlow);

public sealed record ControllerInfo(DeviceInformation Info, string Name, bool Connected);

/// <summary>
/// Runs the whole cycle: monitors the paired controller, removes a "stuck" pairing,
/// searches for the controller in SYNC mode and pairs it again automatically.
/// </summary>
public sealed class ControllerService : IDisposable
{
    static readonly string[] AepProps =
    {
        "System.Devices.Aep.DeviceAddress",
        "System.Devices.Aep.IsConnected",
        "System.Devices.Aep.IsPaired",
    };

    readonly AppSettings _settings;
    readonly SemaphoreSlim _flowLock = new(1, 1);
    readonly CancellationTokenSource _loopCts = new();
    CancellationTokenSource? _flowCts;
    bool _flowIsAuto;
    bool _autoSuspended;
    bool _wasConnected;
    DateTime? _disconnectedSince;
    DateTime _holdUntil = DateTime.MinValue;

    public event Action<ServiceStatus>? StatusChanged;
    public event Action<string>? Log;
    public event Action<string, string>? Notify;

    public ServiceStatus Current { get; private set; } =
        new(LinkState.Starting, FlowStep.None, Loc.T("starting_title"), Loc.T("starting_detail"), true, false);

    public bool FlowRunning => _flowCts != null;

    public ControllerService(AppSettings settings) => _settings = settings;

    public void Start() => _ = Task.Run(() => MonitorLoopAsync(_loopCts.Token));

    // ───────────────────────────── Public API ─────────────────────────────

    /// <summary>Manual procedure: remove, search (90 s) and pair again.</summary>
    public Task ConnectNowAsync()
    {
        _autoSuspended = false;
        _holdUntil = DateTime.MinValue;
        return RunFlowAsync(isAuto: false);
    }

    public void CancelFlow()
    {
        if (_flowCts == null) return;
        if (_flowIsAuto) _autoSuspended = true; // user stopped it: no immediate restart
        _flowCts.Cancel();
    }

    public void OnAutoModeChanged()
    {
        _autoSuspended = false;
        _holdUntil = DateTime.MinValue;
        if (!_settings.AutoMode && _flowIsAuto) _flowCts?.Cancel();
    }

    public async Task RestartBluetoothAsync()
    {
        CancelFlow();
        try
        {
            var access = await Radio.RequestAccessAsync();
            if (access != RadioAccessStatus.Allowed)
            {
                WriteLog(Loc.T("log_radio_denied", access));
                return;
            }
            var radio = await GetRadioAsync();
            if (radio == null) { WriteLog(Loc.T("log_no_radio")); return; }

            WriteLog(Loc.T("log_radio_restart"));
            Publish(LinkState.Removing, FlowStep.None, Loc.T("restarting_title"), Loc.T("restarting_detail"), force: true);
            _holdUntil = DateTime.UtcNow.AddSeconds(5);
            await radio.SetStateAsync(RadioState.Off);
            await Task.Delay(2500);
            await radio.SetStateAsync(RadioState.On);
            WriteLog(Loc.T("log_radio_on"));
            _holdUntil = DateTime.UtcNow.AddSeconds(2);
        }
        catch (Exception ex)
        {
            WriteLog(Loc.T("log_radio_fail", ex.Message));
        }
    }

    // ───────────────────────────── Monitoring ─────────────────────────────

    async Task MonitorLoopAsync(CancellationToken ct)
    {
        WriteLog(Loc.T("log_started", _settings.NameFilter));
        while (!ct.IsCancellationRequested)
        {
            try
            {
                if (!FlowRunning && DateTime.UtcNow >= _holdUntil) await TickAsync();
            }
            catch (Exception ex)
            {
                WriteLog(Loc.T("log_monitor_error", ex.Message));
            }
            try { await Task.Delay(2000, ct); } catch (OperationCanceledException) { break; }
        }
    }

    async Task TickAsync()
    {
        var adapter = await BluetoothAdapter.GetDefaultAsync();
        if (adapter == null)
        {
            Publish(LinkState.NoAdapter, FlowStep.None, Loc.T("no_adapter_title"),
                Loc.T("no_adapter_detail"));
            return;
        }
        var radio = await adapter.GetRadioAsync();
        if (radio != null && radio.State != RadioState.On)
        {
            _disconnectedSince = null;
            Publish(LinkState.BluetoothOff, FlowStep.None, Loc.T("bt_off_title"),
                Loc.T("bt_off_detail"));
            return;
        }

        var paired = await GetPairedControllersAsync();
        var connected = paired.FirstOrDefault(c => c.Connected);
        bool auto = _settings.AutoMode && !_autoSuspended;

        if (connected != null)
        {
            if (!_wasConnected) WriteLog(Loc.T("log_connected", connected.Name));
            _wasConnected = true;
            _disconnectedSince = null;
            _autoSuspended = false;
            Publish(LinkState.Connected, FlowStep.None, Loc.T("connected_title"), Loc.T("connected_detail", connected.Name));
            return;
        }

        if (_wasConnected)
        {
            WriteLog(Loc.T("log_disconnected"));
            _wasConnected = false;
        }

        if (paired.Count > 0)
        {
            _disconnectedSince ??= DateTime.UtcNow;
            int remaining = _settings.GraceSeconds - (int)(DateTime.UtcNow - _disconnectedSince.Value).TotalSeconds;
            if (auto && remaining <= 0)
            {
                WriteLog(Loc.T("log_autorepair"));
                _ = RunFlowAsync(isAuto: true);
                return;
            }
            Publish(LinkState.Disconnected, FlowStep.None, Loc.T("disconnected_title"),
                auto
                    ? Loc.T("disconnected_auto", Math.Max(remaining, 0))
                    : Loc.T("disconnected_manual"));
        }
        else
        {
            _disconnectedSince = null;
            if (auto)
            {
                _ = RunFlowAsync(isAuto: true);
                return;
            }
            Publish(LinkState.NotPaired, FlowStep.None, Loc.T("notpaired_title"),
                _autoSuspended ? Loc.T("notpaired_paused") : Loc.T("notpaired_detail"));
        }
    }

    // ───────────────────────────── Re-pairing procedure ─────────────────────────────

    async Task RunFlowAsync(bool isAuto)
    {
        if (!await _flowLock.WaitAsync(0)) return;
        var cts = CancellationTokenSource.CreateLinkedTokenSource(_loopCts.Token);
        _flowIsAuto = isAuto;
        _flowCts = cts;
        var ct = cts.Token;
        try
        {
            // 1. Remove the existing (stuck) pairing
            Publish(LinkState.Removing, FlowStep.Remove, Loc.T("removing_title"), Loc.T("removing_detail"), force: true);
            var paired = await GetPairedControllersAsync();
            foreach (var c in paired)
            {
                var r = await c.Info.Pairing.UnpairAsync();
                WriteLog(Loc.T("log_removed", c.Name, r.Status));
            }
            if (paired.Count > 0) await Task.Delay(1500, ct);

            // 2. Wait for the controller in SYNC mode
            Publish(LinkState.Searching, FlowStep.Sync,
                Loc.T(isAuto ? "waiting_title_auto" : "waiting_title"),
                Loc.T(isAuto ? "waiting_detail_auto" : "waiting_detail"), force: true);
            WriteLog(Loc.T(isAuto ? "log_search_auto" : "log_search"));

            var device = await DiscoverAsync(isAuto ? null : TimeSpan.FromSeconds(90), dutyCycle: isAuto, ct);
            if (device == null)
            {
                WriteLog(Loc.T("log_notfound"));
                Fail(Loc.T("notfound_title"), Loc.T("notfound_detail"));
                return;
            }
            WriteLog(Loc.T("log_found", device.Name));

            // 3. Pair
            Publish(LinkState.Pairing, FlowStep.Pair, Loc.T("pairing_title"), device.Name, force: true);
            if (!await PairAsync(device, ct))
            {
                Fail(Loc.T("pairfail_title"), Loc.T("pairfail_detail"));
                return;
            }

            // 4. Wait for the connection
            Publish(LinkState.Pairing, FlowStep.Done, Loc.T("connecting_title"), Loc.T("connecting_detail"), force: true);
            bool ok = await WaitConnectedAsync(device.Id, TimeSpan.FromSeconds(20), ct);
            _wasConnected = ok;
            if (ok)
            {
                WriteLog(Loc.T("log_paired_connected", device.Name));
                Publish(LinkState.Connected, FlowStep.Done, Loc.T("connected_title"), Loc.T("connected_detail", device.Name), force: true);
                Notify?.Invoke(Loc.T("connected_title"), Loc.T("notify_connected_text", device.Name));
            }
            else
            {
                WriteLog(Loc.T("log_paired_not_connected"));
                Publish(LinkState.Disconnected, FlowStep.Pair, Loc.T("paired_title"), Loc.T("paired_detail"), force: true);
            }
            _holdUntil = DateTime.UtcNow.AddSeconds(4);
        }
        catch (OperationCanceledException)
        {
            WriteLog(Loc.T("log_cancelled"));
            _holdUntil = DateTime.MinValue;
        }
        catch (Exception ex)
        {
            WriteLog(Loc.T("log_error", ex.Message));
            Fail(Loc.T("error_title"), ex.Message);
        }
        finally
        {
            _flowCts = null;
            _flowIsAuto = false;
            _disconnectedSince = null;
            cts.Dispose();
            _flowLock.Release();
            Current = Current with { Busy = false, AutoFlow = false };
            StatusChanged?.Invoke(Current);
        }
    }

    void Fail(string title, string detail)
    {
        Publish(LinkState.Error, Current.Step, title, detail, force: true);
        _holdUntil = DateTime.UtcNow.AddSeconds(8);
    }

    // ───────────────────────────── Bluetooth (WinRT) ─────────────────────────────

    static async Task<Radio?> GetRadioAsync()
    {
        var adapter = await BluetoothAdapter.GetDefaultAsync();
        return adapter == null ? null : await adapter.GetRadioAsync();
    }

    public async Task<List<ControllerInfo>> GetPairedControllersAsync()
    {
        var selector = BluetoothDevice.GetDeviceSelectorFromPairingState(true);
        var infos = await DeviceInformation.FindAllAsync(selector, AepProps, DeviceInformationKind.AssociationEndpoint);
        var result = new List<ControllerInfo>();
        foreach (var d in infos)
        {
            if (!_settings.Matches(d.Name)) continue;
            bool connected = false;
            try
            {
                using var bt = await BluetoothDevice.FromIdAsync(d.Id);
                connected = bt?.ConnectionStatus == BluetoothConnectionStatus.Connected;
            }
            catch
            {
                if (d.Properties.TryGetValue("System.Devices.Aep.IsConnected", out var v) && v is bool b) connected = b;
            }
            result.Add(new ControllerInfo(d, d.Name, connected));
        }
        return result;
    }

    /// <summary>Searches for an unpaired controller (i.e. in SYNC mode). A null timeout searches forever.</summary>
    async Task<DeviceInformation?> DiscoverAsync(TimeSpan? timeout, bool dutyCycle, CancellationToken ct)
    {
        var deadline = timeout.HasValue ? DateTime.UtcNow + timeout.Value : DateTime.MaxValue;
        while (DateTime.UtcNow < deadline)
        {
            ct.ThrowIfCancellationRequested();
            var slice = TimeSpan.FromSeconds(20);
            if (timeout.HasValue && deadline - DateTime.UtcNow < slice) slice = deadline - DateTime.UtcNow;

            var found = await ScanOnceAsync(slice, ct);
            if (found != null) return found;

            // In automatic mode the search runs in cycles so the radio isn't busy non-stop.
            if (dutyCycle) await Task.Delay(3000, ct);
        }
        return null;
    }

    async Task<DeviceInformation?> ScanOnceAsync(TimeSpan duration, CancellationToken ct)
    {
        var tcs = new TaskCompletionSource<DeviceInformation?>(TaskCreationOptions.RunContinuationsAsynchronously);
        var seen = new Dictionary<string, DeviceInformation>();
        var watcher = DeviceInformation.CreateWatcher(
            BluetoothDevice.GetDeviceSelectorFromPairingState(false), AepProps, DeviceInformationKind.AssociationEndpoint);

        void Check(DeviceInformation d)
        {
            if (_settings.Matches(d.Name) && d.Pairing.CanPair) tcs.TrySetResult(d);
        }

        watcher.Added += (_, d) =>
        {
            lock (seen) seen[d.Id] = d;
            Check(d);
        };
        watcher.Updated += (_, u) =>
        {
            DeviceInformation? d;
            lock (seen)
            {
                if (!seen.TryGetValue(u.Id, out d)) return;
                d.Update(u);
            }
            Check(d);
        };
        watcher.Stopped += (_, _) => tcs.TrySetResult(null);

        watcher.Start();
        try
        {
            var winner = await Task.WhenAny(tcs.Task, Task.Delay(duration, ct));
            ct.ThrowIfCancellationRequested();
            return winner == tcs.Task ? tcs.Task.Result : null;
        }
        finally
        {
            try
            {
                if (watcher.Status is DeviceWatcherStatus.Started or DeviceWatcherStatus.EnumerationCompleted)
                    watcher.Stop();
            }
            catch { }
        }
    }

    async Task<bool> PairAsync(DeviceInformation device, CancellationToken ct)
    {
        const DevicePairingKinds kinds = DevicePairingKinds.ConfirmOnly | DevicePairingKinds.DisplayPin |
                                         DevicePairingKinds.ProvidePin | DevicePairingKinds.ConfirmPinMatch;

        TypedEventHandler<DeviceInformationCustomPairing, DevicePairingRequestedEventArgs> handler = (_, args) =>
        {
            if (args.PairingKind == DevicePairingKinds.ProvidePin) args.Accept("0000");
            else args.Accept();
        };

        for (int attempt = 1; attempt <= 3; attempt++)
        {
            ct.ThrowIfCancellationRequested();
            var custom = device.Pairing.Custom;
            custom.PairingRequested += handler;
            try
            {
                var result = await custom.PairAsync(kinds, DevicePairingProtectionLevel.None);
                WriteLog(Loc.T("log_pair_attempt", attempt, result.Status));
                if (result.Status is DevicePairingResultStatus.Paired or DevicePairingResultStatus.AlreadyPaired)
                    return true;
            }
            finally
            {
                custom.PairingRequested -= handler;
            }
            await Task.Delay(1500, ct);
        }
        return false;
    }

    static async Task<bool> WaitConnectedAsync(string id, TimeSpan timeout, CancellationToken ct)
    {
        var end = DateTime.UtcNow + timeout;
        while (DateTime.UtcNow < end)
        {
            try
            {
                using var bt = await BluetoothDevice.FromIdAsync(id);
                if (bt?.ConnectionStatus == BluetoothConnectionStatus.Connected) return true;
            }
            catch { }
            await Task.Delay(700, ct);
        }
        return false;
    }

    // ───────────────────────────── Utility ─────────────────────────────

    void Publish(LinkState state, FlowStep step, string title, string detail, bool force = false)
    {
        if (!force && (FlowRunning || DateTime.UtcNow < _holdUntil)) return;
        var st = new ServiceStatus(state, step, title, detail, FlowRunning, _flowIsAuto);
        if (st == Current) return;
        Current = st;
        StatusChanged?.Invoke(st);
    }

    void WriteLog(string message)
    {
        Log?.Invoke(message);
        try
        {
            Directory.CreateDirectory(AppSettings.Folder);
            var file = Path.Combine(AppSettings.Folder, "log.txt");
            if (File.Exists(file) && new FileInfo(file).Length > 512 * 1024) File.Delete(file);
            File.AppendAllText(file, $"{DateTime.Now:yyyy-MM-dd HH:mm:ss}  {message}{Environment.NewLine}");
        }
        catch { }
    }

    public void Dispose()
    {
        _loopCts.Cancel();
        _flowCts?.Cancel();
    }
}
