using System;
using System.Collections.Generic;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;
using System.Windows.Media.Effects;
using System.Windows.Shapes;
using Windows.Gaming.Input;

namespace ProControllerConnect;

/// <summary>
/// Vector drawing of the controller with live button mapping,
/// plus a row of "chips" naming each button.
/// </summary>
public sealed class ControllerView : StackPanel
{
    // Button indices in the Pro Controller HID report
    const int B = 0, A = 1, Y = 2, X = 3, L = 4, R = 5, ZL = 6, ZR = 7,
              Minus = 8, Plus = 9, LS = 10, RS = 11, Home = 12, Capture = 13;
    const int Up = 100, Down = 101, Left = 102, Right = 103; // D-pad (from the hat switch)

    static readonly Color AccentColor = Color.FromRgb(0x5B, 0x8C, 0xFF);
    static readonly Brush BodyStroke = Frozen(new SolidColorBrush(Color.FromRgb(0x34, 0x3B, 0x4A)));
    static readonly Brush Well = Frozen(new SolidColorBrush(Color.FromRgb(0x12, 0x15, 0x1B)));
    static readonly Brush IdleFill = Frozen(new LinearGradientBrush(Color.FromRgb(0x33, 0x39, 0x47), Color.FromRgb(0x24, 0x29, 0x33), 90));
    static readonly Brush IdleStroke = Frozen(new SolidColorBrush(Color.FromRgb(0x42, 0x4A, 0x5B)));
    static readonly Brush OnFill = Frozen(new LinearGradientBrush(Color.FromRgb(0x7A, 0xA3, 0xFF), Color.FromRgb(0x4A, 0x74, 0xF0), 90));
    static readonly Brush OnStroke = Frozen(new SolidColorBrush(Color.FromRgb(0x9D, 0xBB, 0xFF)));
    static readonly Brush LabelIdle = Frozen(new SolidColorBrush(Color.FromRgb(0x9A, 0xA2, 0xB2)));
    static readonly Brush ChipIdle = Frozen(new SolidColorBrush(Color.FromRgb(0x22, 0x26, 0x30)));
    static readonly Brush ChipOn = Frozen(new SolidColorBrush(AccentColor));
    static readonly Effect Glow = Frozen(new DropShadowEffect { Color = AccentColor, BlurRadius = 14, ShadowDepth = 0, Opacity = 0.9 });

    readonly Canvas _c = new() { Width = 420, Height = 236 };
    readonly Dictionary<int, List<(Shape shape, TextBlock? label)>> _parts = new();
    readonly Dictionary<int, Border> _chips = new();
    readonly Dictionary<int, bool> _state = new();
    readonly Dictionary<Ellipse, Ellipse> _caps = new();
    readonly Ellipse _leftKnob, _rightKnob;
    readonly Line _leftVector, _rightVector;
    static readonly Point LeftStick = new(116, 100), RightStick = new(254, 150);
    static readonly Point DPad = new(164, 150), Face = new(304, 100);

    public ControllerView()
    {
        Children.Add(new Viewbox { Child = _c, Stretch = Stretch.Uniform, Height = 190 });

        // ── Shoulders: ZL/ZR behind, L/R in front, angled like the real controller
        AddShoulder(ZL, 74, 8, 74, 22, -11, "ZL");
        AddShoulder(ZR, 272, 8, 74, 22, 11, "ZR");
        AddShoulder(L, 56, 30, 98, 20, -11, "L");
        AddShoulder(R, 266, 30, 98, 20, 11, "R");

        // ── Body with grips
        var body = new Path
        {
            Data = Geometry.Parse(
                "M 130,52 L 290,52 C 332,52 360,60 378,84 C 396,108 408,152 412,190 " +
                "C 416,222 396,236 374,230 C 354,224 340,206 326,192 C 314,180 300,174 284,174 " +
                "L 136,174 C 120,174 106,180 94,192 C 80,206 66,224 46,230 C 24,236 4,222 8,190 " +
                "C 12,152 24,108 42,84 C 60,60 88,52 130,52 Z"),
            Fill = new LinearGradientBrush(Color.FromRgb(0x2A, 0x2F, 0x3A), Color.FromRgb(0x1A, 0x1E, 0x26), 90),
            Stroke = BodyStroke,
            StrokeThickness = 1.5,
        };
        _c.Children.Add(body);
        // top highlight
        _c.Children.Add(new Path
        {
            Data = Geometry.Parse("M 132,58 L 288,58 C 326,58 352,66 368,86"),
            Stroke = new SolidColorBrush(Color.FromArgb(0x30, 0xFF, 0xFF, 0xFF)),
            StrokeThickness = 1.2,
        });

        // ── Player LEDs (first one lit: controller connected)
        for (int i = 0; i < 4; i++)
        {
            var led = new Ellipse
            {
                Width = 5, Height = 5,
                Fill = i == 0 ? new SolidColorBrush(Color.FromRgb(0x34, 0xC7, 0x6F)) : new SolidColorBrush(Color.FromRgb(0x30, 0x36, 0x42)),
            };
            if (i == 0) led.Effect = new DropShadowEffect { Color = Color.FromRgb(0x34, 0xC7, 0x6F), BlurRadius = 8, ShadowDepth = 0 };
            Place(led, 196 + i * 8, 160);
            _c.Children.Add(led);
        }

        // ── Stick
        (_leftKnob, _leftVector) = AddStick(LS, LeftStick);
        (_rightKnob, _rightVector) = AddStick(RS, RightStick);

        // ── D-pad
        _c.Children.Add(Circle(DPad, 30, Well, null));
        AddArm(Up, "M 0,-6 L -7,-13 L -7,-25 Q -7,-27 -5,-27 L 5,-27 Q 7,-27 7,-25 L 7,-13 Z", "▲", 0, -20);
        AddArm(Down, "M 0,6 L -7,13 L -7,25 Q -7,27 -5,27 L 5,27 Q 7,27 7,25 L 7,13 Z", "▼", 0, 20);
        AddArm(Left, "M -6,0 L -13,-7 L -25,-7 Q -27,-7 -27,-5 L -27,5 Q -27,7 -25,7 L -13,7 Z", "◀", -20, 0);
        AddArm(Right, "M 6,0 L 13,-7 L 25,-7 Q 27,-7 27,-5 L 27,5 Q 27,7 25,7 L 13,7 Z", "▶", 20, 0);

        // ── ABXY
        _c.Children.Add(Circle(Face, 38, Well, null));
        AddRound(X, new Point(Face.X, Face.Y - 22), 12, "X", 11);
        AddRound(A, new Point(Face.X + 22, Face.Y), 12, "A", 11);
        AddRound(B, new Point(Face.X, Face.Y + 22), 12, "B", 11);
        AddRound(Y, new Point(Face.X - 22, Face.Y), 12, "Y", 11);

        // ── Center buttons
        AddRound(Minus, new Point(184, 82), 8, "−", 10);
        AddRound(Plus, new Point(236, 82), 8, "+", 10);
        var cap = new Rectangle { Width = 15, Height = 15, RadiusX = 3, RadiusY = 3 };
        Place(cap, 184.5, 107.5);
        AddPart(Capture, cap, null);
        var capDot = new Ellipse { Width = 7, Height = 7, Fill = Well, IsHitTestVisible = false };
        Place(capDot, 188.5, 111.5);
        _c.Children.Add(capDot);
        AddRound(Home, new Point(228, 115), 10, "⌂", 11);

        // ── Mapping chips
        var chips = new WrapPanel { HorizontalAlignment = HorizontalAlignment.Center, Margin = new Thickness(0, 8, 0, 0) };
        foreach (var (id, name) in new (int, string)[]
        {
            (A, "A"), (B, "B"), (X, "X"), (Y, "Y"), (L, "L"), (R, "R"), (ZL, "ZL"), (ZR, "ZR"),
            (Minus, "−"), (Plus, "+"), (LS, "L3"), (RS, "R3"), (Home, "Home"), (Capture, Loc.T("chip_capture")),
            (Up, "↑"), (Down, "↓"), (Left, "←"), (Right, "→"),
        })
        {
            var chip = new Border
            {
                Background = ChipIdle, CornerRadius = new CornerRadius(6), Padding = new Thickness(7, 2, 7, 3),
                Margin = new Thickness(2), MinWidth = 24,
                Child = new TextBlock { Text = name, FontSize = 10.5, Foreground = LabelIdle, HorizontalAlignment = HorizontalAlignment.Center },
            };
            _chips[id] = chip;
            chips.Children.Add(chip);
        }
        Children.Add(chips);
    }

    // ───────────── update ─────────────

    public void Update(bool[] buttons, GameControllerSwitchPosition[] switches, double[] axes)
    {
        for (int i = 0; i <= Capture; i++) Set(i, i < buttons.Length && buttons[i]);

        var hat = switches.Length > 0 ? switches[0] : GameControllerSwitchPosition.Center;
        Set(Up, hat is GameControllerSwitchPosition.Up or GameControllerSwitchPosition.UpLeft or GameControllerSwitchPosition.UpRight);
        Set(Down, hat is GameControllerSwitchPosition.Down or GameControllerSwitchPosition.DownLeft or GameControllerSwitchPosition.DownRight);
        Set(Left, hat is GameControllerSwitchPosition.Left or GameControllerSwitchPosition.UpLeft or GameControllerSwitchPosition.DownLeft);
        Set(Right, hat is GameControllerSwitchPosition.Right or GameControllerSwitchPosition.UpRight or GameControllerSwitchPosition.DownRight);

        if (axes.Length >= 4)
        {
            MoveKnob(_leftKnob, _leftVector, LeftStick, axes[0], axes[1]);
            MoveKnob(_rightKnob, _rightVector, RightStick, axes[2], axes[3]);
        }
    }

    void Set(int id, bool on)
    {
        if (_state.TryGetValue(id, out var old) && old == on) return;
        _state[id] = on;
        if (_parts.TryGetValue(id, out var parts))
            foreach (var (shape, label) in parts)
            {
                shape.Fill = on ? OnFill : IdleFill;
                shape.Stroke = on ? OnStroke : IdleStroke;
                shape.Effect = on ? Glow : null;
                if (label != null) label.Foreground = on ? Brushes.White : LabelIdle;
            }
        if (_chips.TryGetValue(id, out var chip))
        {
            chip.Background = on ? ChipOn : ChipIdle;
            ((TextBlock)chip.Child).Foreground = on ? Brushes.White : LabelIdle;
        }
    }

    void MoveKnob(Ellipse knob, Line vector, Point center, double ax, double ay)
    {
        const double travel = 14;
        double x = Math.Clamp(ax * 2 - 1, -1, 1), y = Math.Clamp(ay * 2 - 1, -1, 1);
        if (Math.Abs(x) < 0.06) x = 0;
        if (Math.Abs(y) < 0.06) y = 0;
        double px = center.X + x * travel, py = center.Y + y * travel;
        Canvas.SetLeft(knob, px - knob.Width / 2);
        Canvas.SetTop(knob, py - knob.Height / 2);
        vector.X2 = px; vector.Y2 = py;
        if (_caps.TryGetValue(knob, out var cap)) { Canvas.SetLeft(cap, px - 11); Canvas.SetTop(cap, py - 11); }
        vector.Opacity = Math.Min(1, Math.Sqrt(x * x + y * y) * 1.5);
    }

    // ───────────── construction ─────────────

    (Ellipse knob, Line vector) AddStick(int id, Point c)
    {
        _c.Children.Add(Circle(c, 34, Well, IdleStroke));
        _c.Children.Add(Circle(c, 22, null, new SolidColorBrush(Color.FromArgb(0x40, 0x5B, 0x8C, 0xFF)), 1, dash: true));
        var vector = new Line
        {
            X1 = c.X, Y1 = c.Y, X2 = c.X, Y2 = c.Y, Stroke = new SolidColorBrush(AccentColor),
            StrokeThickness = 2, StrokeEndLineCap = PenLineCap.Round, Opacity = 0,
        };
        _c.Children.Add(vector);
        var knob = new Ellipse { Width = 34, Height = 34, StrokeThickness = 1.5 };
        Place(knob, c.X - 17, c.Y - 17);
        AddPart(id, knob, null);
        var cap = new Ellipse { Width = 22, Height = 22, Stroke = new SolidColorBrush(Color.FromArgb(0x50, 0x00, 0x00, 0x00)), StrokeThickness = 1, IsHitTestVisible = false };
        Place(cap, c.X - 11, c.Y - 11);
        _caps[knob] = cap; // the cap follows the knob
        _c.Children.Add(cap);
        return (knob, vector);
    }

    void AddShoulder(int id, double x, double y, double w, double h, double angle, string text)
    {
        var rect = new Rectangle { Width = w, Height = h, RadiusX = h / 2, RadiusY = h / 2 };
        Place(rect, x, y);
        rect.RenderTransform = new RotateTransform(angle, w / 2, h / 2);
        var label = new TextBlock
        {
            Text = text, FontSize = 10.5, FontWeight = FontWeights.SemiBold, Width = w,
            TextAlignment = TextAlignment.Center, IsHitTestVisible = false,
            RenderTransform = new RotateTransform(angle, w / 2, h / 2),
        };
        Place(label, x, y + (h - 14) / 2);
        AddPart(id, rect, label);
    }

    void AddArm(int id, string geometry, string glyph, double gx, double gy)
    {
        var arm = new Path { Data = Geometry.Parse(geometry), StrokeThickness = 1 };
        Place(arm, DPad.X, DPad.Y);
        var label = new TextBlock { Text = glyph, FontSize = 7, Width = 14, TextAlignment = TextAlignment.Center, IsHitTestVisible = false };
        Place(label, DPad.X + gx - 7, DPad.Y + gy - 5);
        AddPart(id, arm, label);
    }

    void AddRound(int id, Point c, double r, string text, double fontSize)
    {
        var e = new Ellipse { Width = r * 2, Height = r * 2 };
        Place(e, c.X - r, c.Y - r);
        var label = new TextBlock
        {
            Text = text, FontSize = fontSize, FontWeight = FontWeights.Bold, Width = r * 2,
            TextAlignment = TextAlignment.Center, IsHitTestVisible = false,
        };
        Place(label, c.X - r, c.Y - fontSize * 0.68);
        AddPart(id, e, label);
    }

    void AddPart(int id, Shape shape, TextBlock? label)
    {
        shape.Fill = IdleFill;
        shape.Stroke = IdleStroke;
        if (shape.StrokeThickness == 1 || shape.StrokeThickness == 0) shape.StrokeThickness = 1;
        _c.Children.Add(shape);
        if (label != null)
        {
            label.Foreground = LabelIdle;
            _c.Children.Add(label);
        }
        if (!_parts.TryGetValue(id, out var list)) _parts[id] = list = new();
        list.Add((shape, label));
    }

    static Ellipse Circle(Point c, double r, Brush? fill, Brush? stroke, double thickness = 1, bool dash = false)
    {
        var e = new Ellipse { Width = r * 2, Height = r * 2, Fill = fill, Stroke = stroke, StrokeThickness = thickness, IsHitTestVisible = false };
        if (dash) e.StrokeDashArray = new DoubleCollection { 2, 3 };
        Place(e, c.X - r, c.Y - r);
        return e;
    }

    static void Place(UIElement e, double x, double y)
    {
        Canvas.SetLeft(e, x);
        Canvas.SetTop(e, y);
    }

    static T Frozen<T>(T f) where T : Freezable
    {
        f.Freeze();
        return f;
    }
}
