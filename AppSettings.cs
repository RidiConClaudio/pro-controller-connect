using System;
using System.IO;
using System.Linq;
using System.Text.Json;
using Microsoft.Win32;

namespace ProControllerConnect;

public sealed class AppSettings
{
    /// <summary>Ripara e riabbina automaticamente il controller.</summary>
    public bool AutoMode { get; set; } = true;
    public bool StartWithWindows { get; set; } = false;
    public bool StartMinimized { get; set; } = true;
    public bool Notifications { get; set; } = true;

    /// <summary>Nomi dei dispositivi da gestire, separati da ';'.</summary>
    public string NameFilter { get; set; } = "Pro Controller";

    /// <summary>Secondi di attesa prima di rimuovere un abbinamento "bloccato".</summary>
    public int GraceSeconds { get; set; } = 15;

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
        catch { /* file corrotto: si riparte dai default */ }
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
