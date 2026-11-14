using System;
using System.Linq;
using System.Threading;
using System.Windows;

namespace ProControllerConnect;

public partial class App : Application
{
    const string MutexName = @"Local\ProControllerConnect.Single";
    const string ShowEventName = @"Local\ProControllerConnect.Show";

    Mutex? _mutex;
    EventWaitHandle? _showEvent;
    MainWindow? _window;

    protected override void OnStartup(StartupEventArgs e)
    {
        // Una sola istanza: se è già aperta, la porta in primo piano.
        _mutex = new Mutex(true, MutexName, out bool created);
        if (!created)
        {
            try { EventWaitHandle.OpenExisting(ShowEventName).Set(); } catch { }
            Shutdown();
            return;
        }
        _showEvent = new EventWaitHandle(false, EventResetMode.AutoReset, ShowEventName);
        new Thread(() =>
        {
            while (_showEvent.WaitOne()) Dispatcher.BeginInvoke(() => _window?.ShowFromTray());
        }) { IsBackground = true }.Start();

        DispatcherUnhandledException += (_, args) => args.Handled = true;

        base.OnStartup(e);

        var settings = AppSettings.Load();
        settings.ApplyStartup(); // aggiorna il percorso nel registro se l'exe è stato spostato
        var service = new ControllerService(settings);
        _window = new MainWindow(service, settings);

        bool minimized = e.Args.Any(a => a.Equals("--minimized", StringComparison.OrdinalIgnoreCase));
        if (!minimized) _window.Show();

        service.Start();
    }

    protected override void OnExit(ExitEventArgs e)
    {
        _mutex?.Dispose();
        base.OnExit(e);
    }
}
