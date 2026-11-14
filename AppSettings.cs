using System;
using System.IO;
using System.Linq;
using System.Text.Json;
using Microsoft.Win32;

namespace ProControllerConnect;

public sealed class AppSettings
{
    /// <summary>Automatically repairs and re-pairs the controller.</summary>
    public bool AutoMode { get; set; } = true;
    public bool StartWithWindows { get; set; } = false;
    public bool StartMinimized { get; set; } = true;
    public bool Notifications { get; set; } = true;

    /// <summary>Device names to manage, separated by ';'.</summary>
    public string NameFilter { get; set; } = "Pro Controller";

    /// <summary>Seconds to wait before removing a "stuck" pairing.</summary>
    public int GraceSeconds { get; set; } = 15;

    /// <summary>"auto" (Windows language) or a code such as "en", "it".</summary>
    public string Language { get; set; } = "auto";

    public bool HideTipShown { get; set; }

    public static string Folder =>
        Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "ProControllerConnect");

    static string FilePath => Path.Combine(Folder, "settings.json");

    public bool Matches(string? deviceName)
    {
        if (string.IsNullOrWhiteSpace(deviceName)) return false;
        return NameFilter.Split(';', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries)
            .Any(f => deviceName.Contains(f, StringComparison.OrdinalIgnoreCase));
    }

    public static AppSettings Load()
    {
        try
        {
            if (File.Exists(FilePath))
                return JsonSerializer.Deserialize<AppSettings>(File.ReadAllText(FilePath)) ?? new AppSettings();
        }
        catch { /* corrupted file: fall back to defaults */ }
        return new AppSettings();
    }

    public void Save()
    {
        try
        {
            Directory.CreateDirectory(Folder);
            File.WriteAllText(FilePath, JsonSerializer.Serialize(this, new JsonSerializerOptions { WriteIndented = true }));
        }
        catch { }
    }

    public void ApplyStartup()
    {
        const string runKey = @"Software\Microsoft\Windows\CurrentVersion\Run";
        const string valueName = "ProControllerConnect";
        try
        {
            using var key = Registry.CurrentUser.OpenSubKey(runKey, true) ?? Registry.CurrentUser.CreateSubKey(runKey);
            if (StartWithWindows && Environment.ProcessPath is { } exe)
                key.SetValue(valueName, $"\"{exe}\"" + (StartMinimized ? " --minimized" : ""));
            else
                key.DeleteValue(valueName, false);
        }
        catch { }
    }
}
