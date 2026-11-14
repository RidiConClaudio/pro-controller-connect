using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Media.Animation;
using Drawing = System.Drawing;
using WinForms = System.Windows.Forms;

namespace ProControllerConnect;

public partial class MainWindow : Window
{
    static readonly Color Green = Color.FromRgb(0x34, 0xC7, 0x6F);
    static readonly Color Accent = Color.FromRgb(0x5B, 0x8C, 0xFF);
    static readonly Color Amber = Color.FromRgb(0xFF, 0xB0, 0x20);
    static readonly Color Red = Color.FromRgb(0xFF, 0x5A, 0x5F);
    static readonly Color Gray = Color.FromRgb(0x8B, 0x93, 0xA3);

    static string[] StepLabels => new[] { Loc.T("step_remove"), Loc.T("step_sync"), Loc.T("step_pair"), Loc.T("step_connected") };

    readonly ControllerService _svc;
    readonly AppSettings _settings;
    readonly WinForms.NotifyIcon _tray;
    readonly WinForms.ToolStripMenuItem _trayConnect;
    readonly WinForms.ToolStripMenuItem _trayAuto;
    readonly Dictionary<Color, Drawing.Icon> _iconCache = new();
    readonly Border[] _dots = new Border[4];
    readonly TextBlock[] _dotText = new TextBlock[4];
    readonly TextBlock[] _labels = new TextBlock[4];
    bool _exiting;
    bool _pulsing;

    public MainWindow(ControllerService service, AppSettings settings)
    {
        _svc = service;
        _settings = settings;
        InitializeComponent();
        BuildStepper();

        AutoModeSwitch.IsChecked = settings.AutoMode;
        StartupSwitch.IsChecked = settings.StartWithWindows;
        MinimizedSwitch.IsChecked = settings.StartMinimized;
        NotifySwitch.IsChecked = settings.Notifications;
        BuildLanguagePicker();

        // Notification area icon
        _trayConnect = new WinForms.ToolStripMenuItem(Loc.T("btn_connect"), null, (_, _) => MainButton_Click(this, new RoutedEventArgs()));
        _trayAuto = new WinForms.ToolStripMenuItem(Loc.T("set_auto_title"), null, (_, _) =>
        {
            AutoModeSwitch.IsChecked = !_settings.AutoMode;
            Setting_Click(AutoModeSwitch, new RoutedEventArgs());
        }) { Checked = settings.AutoMode };
        var menu = new WinForms.ContextMenuStrip { Renderer = new DarkMenuRenderer(), ShowImageMargin = false, ShowCheckMargin = true };
        menu.Items.Add(new WinForms.ToolStripMenuItem(Loc.T("tray_open"), null, (_, _) => ShowFromTray()) { Font = new Drawing.Font("Segoe UI", 9f, Drawing.FontStyle.Bold) });
        menu.Items.Add(_trayConnect);
        menu.Items.Add(_trayAuto);
        menu.Items.Add(new WinForms.ToolStripSeparator());
        menu.Items.Add(new WinForms.ToolStripMenuItem(Loc.T("tray_exit"), null, (_, _) => ExitApp()));
        foreach (WinForms.ToolStripItem it in menu.Items) { it.ForeColor = Drawing.Color.FromArgb(236, 238, 242); it.Padding = new WinForms.Padding(6, 4, 6, 4); }

        _tray = new WinForms.NotifyIcon
        {
            Icon = GetTrayIcon(Gray),
            Text = "Pro Controller Connect",
            ContextMenuStrip = menu,
            Visible = true,
        };
        _tray.MouseClick += (_, e) => { if (e.Button == WinForms.MouseButtons.Left) ShowFromTray(); };

        _svc.StatusChanged += st => Dispatcher.BeginInvoke(() => ApplyStatus(st));
        _svc.Log += msg => Dispatcher.BeginInvoke(() => AppendLog(msg));
        _svc.Notify += (title, text) => Dispatcher.BeginInvoke(() =>
        {
            if (_settings.Notifications) _tray.ShowBalloonTip(4000, title, text, WinForms.ToolTipIcon.Info);
        });

        _input.ControllerChanged += () => Dispatcher.BeginInvoke(UpdateTester);
        CompositionTarget.Rendering += OnRender;

        ApplyStatus(_svc.Current);
    }

    // ───────────────────────────── Live test ─────────────────────────────

    readonly InputMonitor _input = new();
    bool _connected;
    bool _inputVerified;
    DateTime _lastInput = DateTime.MinValue;
    DateTime _lastTextUpdate = DateTime.MinValue;
    bool[] _prevButtons = Array.Empty<bool>();
    double[] _prevAxes = Array.Empty<double>();
    Windows.Gaming.Input.GameControllerSwitchPosition _prevHat;

    void UpdateTester()
    {
        if (TesterCard == null) return;
        bool show = _connected;
        TesterCard.Visibility = show ? Visibility.Visible : Visibility.Collapsed;
        StepperCard.Visibility = show ? Visibility.Collapsed : Visibility.Visible;
        if (!show)
        {
            _inputVerified = false;
            _lastInput = DateTime.MinValue;
        }
        DeviceInfo.Text = _input.Controller != null
            ? Loc.T("device_detected", _input.Describe())
            : Loc.T("device_waiting");
        UpdateBadge();
    }

    void UpdateBadge()
    {
        Color c;
        string text;
        if (_input.Controller == null) { c = Amber; text = Loc.T("badge_notdetected"); }
        else if (_inputVerified && (DateTime.UtcNow - _lastInput).TotalSeconds < 0.25) { c = Accent; text = Loc.T("badge_receiving"); }
        else if (_inputVerified) { c = Green; text = Loc.T("badge_works"); }
        else { c = Gray; text = Loc.T("badge_press"); }
        InputDot.Fill = new SolidColorBrush(c);
        InputBadge.Background = new SolidColorBrush(Color.FromArgb(0x30, c.R, c.G, c.B));
        InputBadgeText.Text = text;
    }

    void OnRender(object? sender, EventArgs e)
    {
        if (!IsVisible || TesterCard.Visibility != Visibility.Visible || TabController.IsChecked != true) return;
        if (!_input.Read()) return;
#if DEBUG
        if (Environment.GetEnvironmentVariable("PCC_DEMO") == "1") // visual preview without pressing buttons
        {
            int t = (int)(DateTime.Now.TimeOfDay.TotalSeconds * 2);
            for (int i = 0; i < _input.Buttons.Length; i++) _input.Buttons[i] = i == t % 14 || i == 1 || i == 6;
            if (_input.Switches.Length > 0) _input.Switches[0] = Windows.Gaming.Input.GameControllerSwitchPosition.UpRight;
            if (_input.Axes.Length >= 4) { _input.Axes[0] = 0.9; _input.Axes[1] = 0.2; _input.Axes[2] = 0.3; _input.Axes[3] = 0.85; }
        }
#endif

        var buttons = _input.Buttons;
        var axes = _input.Axes;
        var hat = _input.Switches.Length > 0 ? _input.Switches[0] : default;

        bool changed = hat != _prevHat || !buttons.AsSpan().SequenceEqual(_prevButtons);
        if (_prevAxes.Length == axes.Length)
            for (int i = 0; i < axes.Length; i++)
                if (Math.Abs(axes[i] - _prevAxes[i]) > 0.04) changed = true;

        if (changed)
        {
            if (_prevButtons.Length > 0 || _prevAxes.Length > 0) // don't count the first reading
            {
                _lastInput = DateTime.UtcNow;
                if (!_inputVerified) { _inputVerified = true; AppendLog(Loc.T("log_input_ok")); }
            }
            _prevButtons = (bool[])buttons.Clone();
            _prevAxes = (double[])axes.Clone();
            _prevHat = hat;
            Pad.Update(buttons, _input.Switches, axes);
        }

        if ((DateTime.UtcNow - _lastTextUpdate).TotalMilliseconds < 100) return;
        _lastTextUpdate = DateTime.UtcNow;
        if (axes.Length >= 4)
            AxisText.Text = $"L {Fmt(axes[0])} {Fmt(-(axes[1] * 2 - 1), true)}   R {Fmt(axes[2])} {Fmt(-(axes[3] * 2 - 1), true)}";
        if (_inputVerified)
        {
            var ago = (DateTime.UtcNow - _lastInput).TotalSeconds;
            LastInputText.Text = ago < 1.5 ? Loc.T("last_input_now") : Loc.T("last_input_ago", FormatAgo(ago));
        }
        else LastInputText.Text = "";
        UpdateBadge();
    }

    static string Fmt(double v, bool already = false)
    {
        double x = already ? v : v * 2 - 1;
        if (Math.Abs(x) < 0.08) x = 0;
        return x.ToString("+0.00;-0.00; 0.00");
    }

    static string FormatAgo(double s) => s < 60 ? $"{(int)s} s" : s < 3600 ? $"{(int)(s / 60)} min" : $"{(int)(s / 3600)} h";

    void BuildLanguagePicker()
    {
        foreach (var (code, name) in Loc.Languages)
        {
            var rb = new RadioButton
            {
                Style = (Style)FindResource("Segment"),
                GroupName = "Lang",
                Content = name,
                FontSize = 12,
                IsChecked = string.Equals(_settings.Language, code, StringComparison.OrdinalIgnoreCase),
                ToolTip = code == "auto" ? Loc.T("set_lang_desc") : null,
            };
            rb.Checked += (_, _) =>
            {
                if (string.Equals(_settings.Language, code, StringComparison.OrdinalIgnoreCase)) return;
                _settings.Language = code;
                _settings.Save();
                RestartApp();
            };
            LangPanel.Children.Add(rb);
        }
    }

    void RestartApp()
    {
        ((App)Application.Current).ReleaseSingleInstance();
        var exe = Environment.ProcessPath!;
        var psi = new ProcessStartInfo(exe) { UseShellExecute = false };
        if (Path.GetFileNameWithoutExtension(exe).Equals("dotnet", StringComparison.OrdinalIgnoreCase))
            psi.ArgumentList.Add(System.Reflection.Assembly.GetEntryAssembly()!.Location);
        try { Process.Start(psi); } catch { }
        ExitApp();
    }

    void Tab_Checked(object sender, RoutedEventArgs e)
    {
        if (ControllerPage == null || SettingsPage == null) return;
        bool ctrl = TabController.IsChecked == true;
        ControllerPage.Visibility = ctrl ? Visibility.Visible : Visibility.Hidden;
        SettingsPage.Visibility = ctrl ? Visibility.Hidden : Visibility.Visible;
    }

    // ───────────────────────────── Status → UI ─────────────────────────────

    void ApplyStatus(ServiceStatus st)
    {
        var (color, glyph, badge, pulse) = st.State switch
        {
            LinkState.Connected => (Green, "", "", false),
            LinkState.Searching => (Accent, "", "", true),
            LinkState.Pairing or LinkState.Removing => (Accent, "", "", true),
            LinkState.Disconnected => (Amber, "", "", false),
            LinkState.BluetoothOff or LinkState.NoAdapter => (Red, "", "", false),
            LinkState.Error => (Red, "", "", false),
            LinkState.Starting => (Gray, "", "", true),
            _ => (Gray, "", "", false),
        };

        var brush = new SolidColorBrush(color);
        StatusGlyph.Text = glyph;
        StatusGlyph.Foreground = brush;
        Glow.Fill = brush;
        PulseRing.Stroke = brush;
        Badge.Background = brush;
        BadgeGlyph.Text = badge;
        Badge.Visibility = string.IsNullOrEmpty(badge) ? Visibility.Collapsed : Visibility.Visible;
        SetPulse(pulse);

        StatusTitle.Text = st.Title;
        StatusDetail.Text = st.Detail;

        // Main button
        if (st.Busy)
        {
            MainButton.Style = (Style)FindResource("SecondaryButton");
            MainButtonGlyph.Text = "";
            MainButtonText.Text = Loc.T(st.AutoFlow ? "btn_stop_search" : "btn_cancel");
        }
        else if (st.State == LinkState.Connected)
        {
            MainButton.Style = (Style)FindResource("SecondaryButton");
            MainButtonGlyph.Text = "";
            MainButtonText.Text = Loc.T("btn_repair");
        }
        else
        {
            MainButton.Style = (Style)FindResource("PrimaryButton");
            MainButtonGlyph.Text = "";
            MainButtonText.Text = Loc.T("btn_connect");
        }
        MainButton.IsEnabled = st.State is not (LinkState.BluetoothOff or LinkState.NoAdapter) || st.Busy;

        UpdateStepper(st);
        bool connected = st.State == LinkState.Connected;
        if (connected != _connected) { _connected = connected; UpdateTester(); }

        // Notification area
        _tray.Icon = GetTrayIcon(color);
        var tip = "Pro Controller Connect · " + st.Title;
        _tray.Text = tip.Length > 63 ? tip[..63] : tip;
        _trayConnect.Text = Loc.T(st.Busy ? "btn_cancel" : "btn_connect");
    }

    void BuildStepper()
    {
        for (int i = 0; i < 4; i++)
        {
            _dotText[i] = new TextBlock
            {
                Text = (i + 1).ToString(), FontSize = 12, FontWeight = FontWeights.SemiBold,
                HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center,
            };
            _dots[i] = new Border
            {
                Width = 28, Height = 28, CornerRadius = new CornerRadius(14), BorderThickness = new Thickness(2),
                Child = _dotText[i], HorizontalAlignment = HorizontalAlignment.Center,
            };
            _labels[i] = new TextBlock
            {
                Text = StepLabels[i], FontSize = 11.5, Margin = new Thickness(0, 7, 0, 0),
                HorizontalAlignment = HorizontalAlignment.Center,
            };
            var panel = new StackPanel();
            panel.Children.Add(_dots[i]);
            panel.Children.Add(_labels[i]);
            Stepper.Children.Add(panel);
        }
    }

    void UpdateStepper(ServiceStatus st)
    {
        var card = (Brush)FindResource("CardBrush");
        var line = (Brush)FindResource("LineBrush");
        var sub = (Brush)FindResource("SubTextBrush");
        var text = (Brush)FindResource("TextBrush");
        int current = (int)st.Step;
        bool success = st.State == LinkState.Connected && st.Step == FlowStep.Done;
        bool error = st.State == LinkState.Error;
        var icon = (FontFamily)FindResource("IconFont");
        var ui = (FontFamily)FindResource("UiFont");

        for (int i = 0; i < 4; i++)
        {
            int n = i + 1;
            var dot = _dots[i];
            var t = _dotText[i];
            if (st.Step != FlowStep.None && (n < current || success))
            {
                var c = new SolidColorBrush(success ? Green : Accent);
                dot.Background = c; dot.BorderBrush = c;
                t.FontFamily = icon; t.Text = ""; t.FontSize = 11; t.Foreground = Brushes.White;
                _labels[i].Foreground = text;
            }
            else if (st.Step != FlowStep.None && n == current)
            {
                var c = new SolidColorBrush(error ? Red : Accent);
                dot.Background = card; dot.BorderBrush = c;
                t.FontFamily = error ? icon : ui; t.Text = error ? "" : n.ToString(); t.FontSize = error ? 10 : 12;
                t.Foreground = c;
                _labels[i].Foreground = text;
            }
            else
            {
                dot.Background = card; dot.BorderBrush = line;
                t.FontFamily = ui; t.Text = n.ToString(); t.FontSize = 12; t.Foreground = sub;
                _labels[i].Foreground = sub;
            }
        }
    }

    void SetPulse(bool on)
    {
        if (on == _pulsing) return;
        _pulsing = on;
        if (on)
        {
            var dur = TimeSpan.FromSeconds(1.6);
            var scale = new DoubleAnimation(1.0, 1.32, dur) { RepeatBehavior = RepeatBehavior.Forever, EasingFunction = new CubicEase { EasingMode = EasingMode.EaseOut } };
            var fade = new DoubleAnimation(0.9, 0.0, dur) { RepeatBehavior = RepeatBehavior.Forever, EasingFunction = new CubicEase { EasingMode = EasingMode.EaseOut } };
            PulseScale.BeginAnimation(ScaleTransform.ScaleXProperty, scale);
            PulseScale.BeginAnimation(ScaleTransform.ScaleYProperty, scale);
            PulseRing.BeginAnimation(OpacityProperty, fade);
        }
        else
        {
            PulseScale.BeginAnimation(ScaleTransform.ScaleXProperty, null);
            PulseScale.BeginAnimation(ScaleTransform.ScaleYProperty, null);
            PulseRing.BeginAnimation(OpacityProperty, null);
            PulseRing.Opacity = 0;
        }
    }

    void AppendLog(string message)
    {
        LogBox.AppendText($"{DateTime.Now:HH:mm:ss}  {message}\n");
        if (LogBox.LineCount > 400) LogBox.Text = LogBox.Text[(LogBox.Text.Length / 2)..];
        LogBox.ScrollToEnd();
    }

    // ───────────────────────────── UI events ─────────────────────────────

    void MainButton_Click(object sender, RoutedEventArgs e)
    {
        if (_svc.FlowRunning) _svc.CancelFlow();
        else _ = _svc.ConnectNowAsync();
    }

    void Setting_Click(object sender, RoutedEventArgs e)
    {
        bool autoBefore = _settings.AutoMode;
        _settings.AutoMode = AutoModeSwitch.IsChecked == true;
        _settings.StartWithWindows = StartupSwitch.IsChecked == true;
        _settings.StartMinimized = MinimizedSwitch.IsChecked == true;
        _settings.Notifications = NotifySwitch.IsChecked == true;
        _settings.Save();
        _settings.ApplyStartup();
        _trayAuto.Checked = _settings.AutoMode;
        if (autoBefore != _settings.AutoMode) _svc.OnAutoModeChanged();
    }

    void RestartBt_Click(object sender, RoutedEventArgs e) => _ = _svc.RestartBluetoothAsync();

    void BtSettings_Click(object sender, RoutedEventArgs e)
    {
        try { Process.Start(new ProcessStartInfo("ms-settings:bluetooth") { UseShellExecute = true }); } catch { }
    }

    void Log_Click(object sender, RoutedEventArgs e)
    {
        bool show = LogPanel.Visibility != Visibility.Visible;
        LogPanel.Visibility = show ? Visibility.Visible : Visibility.Collapsed;
        LogButtonText.Text = Loc.T(show ? "tool_log_hide" : "tool_log");
        if (show) LogBox.ScrollToEnd();
    }

    void Minimize_Click(object sender, RoutedEventArgs e) => WindowState = WindowState.Minimized;

    void Close_Click(object sender, RoutedEventArgs e) => Close();

    protected override void OnClosing(CancelEventArgs e)
    {
        if (!_exiting)
        {
            e.Cancel = true;
            Hide();
            if (!_settings.HideTipShown)
            {
                _tray.ShowBalloonTip(4000, "Pro Controller Connect", Loc.T("hide_tip"), WinForms.ToolTipIcon.Info);
                _settings.HideTipShown = true;
                _settings.Save();
            }
        }
        base.OnClosing(e);
    }

    public void ShowFromTray()
    {
        Show();
        if (WindowState == WindowState.Minimized) WindowState = WindowState.Normal;
        Activate();
        Topmost = true;
        Topmost = false;
        Focus();
    }

    void ExitApp()
    {
        _exiting = true;
        _svc.Dispose();
        _tray.Visible = false;
        _tray.Dispose();
        Application.Current.Shutdown();
    }

    // ───────────────────────────── Windows 11 window ─────────────────────────────

    [DllImport("dwmapi.dll")]
    static extern int DwmSetWindowAttribute(IntPtr hwnd, int attribute, ref int value, int size);

    [DllImport("user32.dll")]
    static extern bool DestroyIcon(IntPtr handle);

    protected override void OnSourceInitialized(EventArgs e)
    {
        base.OnSourceInitialized(e);
        var hwnd = new WindowInteropHelper(this).Handle;
        int dark = 1, round = 2, border = unchecked((int)0xFFFFFFFE); // DWMWA_COLOR_NONE
        DwmSetWindowAttribute(hwnd, 20, ref dark, sizeof(int));   // dark mode
        DwmSetWindowAttribute(hwnd, 33, ref round, sizeof(int));  // rounded corners
        DwmSetWindowAttribute(hwnd, 34, ref border, sizeof(int)); // no colored border
    }

    Drawing.Icon GetTrayIcon(Color c)
    {
        if (_iconCache.TryGetValue(c, out var cached)) return cached;
        const int size = 64;
        using var bmp = new Drawing.Bitmap(size, size, System.Drawing.Imaging.PixelFormat.Format32bppArgb);
        using (var g = Drawing.Graphics.FromImage(bmp))
        {
            g.SmoothingMode = Drawing.Drawing2D.SmoothingMode.AntiAlias;
            g.TextRenderingHint = Drawing.Text.TextRenderingHint.AntiAliasGridFit;
            using var fill = new Drawing.SolidBrush(Drawing.Color.FromArgb(c.R, c.G, c.B));
            g.FillEllipse(fill, 1, 1, size - 2, size - 2);
            string fontName = new Drawing.Text.InstalledFontCollection().Families
                .Any(f => f.Name == "Segoe Fluent Icons") ? "Segoe Fluent Icons" : "Segoe MDL2 Assets";
            using var font = new Drawing.Font(fontName, size * 0.5f, Drawing.GraphicsUnit.Pixel);
            using var sf = new Drawing.StringFormat { Alignment = Drawing.StringAlignment.Center, LineAlignment = Drawing.StringAlignment.Center };
            g.DrawString("", font, Drawing.Brushes.White, new Drawing.RectangleF(0, 2, size, size), sf);
        }
        IntPtr h = bmp.GetHicon();
        var icon = (Drawing.Icon)Drawing.Icon.FromHandle(h).Clone();
        DestroyIcon(h);
        _iconCache[c] = icon;
        return icon;
    }
}

/// <summary>Dark context menu for the notification area icon.</summary>
sealed class DarkMenuRenderer : WinForms.ToolStripProfessionalRenderer
{
    public DarkMenuRenderer() : base(new DarkColors()) { RoundedEdges = false; }

    sealed class DarkColors : WinForms.ProfessionalColorTable
    {
        static readonly Drawing.Color Bg = Drawing.Color.FromArgb(28, 31, 39);
        static readonly Drawing.Color Hover = Drawing.Color.FromArgb(44, 49, 61);
        static readonly Drawing.Color Line = Drawing.Color.FromArgb(48, 53, 66);
        public override Drawing.Color ToolStripDropDownBackground => Bg;
        public override Drawing.Color MenuBorder => Line;
        public override Drawing.Color MenuItemBorder => Hover;
        public override Drawing.Color MenuItemSelected => Hover;
        public override Drawing.Color ImageMarginGradientBegin => Bg;
        public override Drawing.Color ImageMarginGradientMiddle => Bg;
        public override Drawing.Color ImageMarginGradientEnd => Bg;
        public override Drawing.Color SeparatorDark => Line;
        public override Drawing.Color SeparatorLight => Bg;
        public override Drawing.Color CheckBackground => Hover;
        public override Drawing.Color CheckSelectedBackground => Hover;
        public override Drawing.Color CheckPressedBackground => Hover;
    }
}
