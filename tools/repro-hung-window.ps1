<#
.SYNOPSIS
    Kana Cop の「応答なしウィンドウに巻き込まれる」不具合の再現用スクリプト。

.DESCRIPTION
    UI スレッドをわざと Sleep させて「応答なし」状態になるウィンドウを表示する。
    このウィンドウが前面にある状態で Kana Cop が IME を操作したときに、相手が応答するまで
    待たされないか（他のウィンドウの矯正が止まらないか）を確認できる。

    Windows のゴースト化（応答なしウィンドウを別ウィンドウに差し替える機能）が働くと
    前面ウィンドウのハンドルが変わって再現条件がぶれるため、このプロセスでは無効化している。

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File tools\repro-hung-window.ps1
    powershell -ExecutionPolicy Bypass -File tools\repro-hung-window.ps1 -FreezeSeconds 60 -DelaySeconds 5
#>
param(
    # 応答なしにしておく秒数
    [int]$FreezeSeconds = 30,
    # ボタンを押してからフリーズするまでの猶予秒数（その間に IME の状態を整える）
    [int]$DelaySeconds = 3
)

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System.Runtime.InteropServices;
public static class GhostingControl {
    [DllImport("user32.dll")]
    public static extern void DisableProcessWindowsGhosting();
}
"@

[GhostingControl]::DisableProcessWindowsGhosting()

$form = New-Object System.Windows.Forms.Form
$form.Text = "Hung Window Repro (PID $PID)"
$form.Size = New-Object System.Drawing.Size(520, 320)
$form.StartPosition = "CenterScreen"
$form.TopMost = $true

$status = New-Object System.Windows.Forms.Label
$status.Location = New-Object System.Drawing.Point(12, 12)
$status.Size = New-Object System.Drawing.Size(480, 40)
$status.Font = New-Object System.Drawing.Font("Meiryo UI", 11)
$status.Text = "待機中: ボタンを押すと ${DelaySeconds} 秒後に ${FreezeSeconds} 秒間フリーズします"

$button = New-Object System.Windows.Forms.Button
$button.Location = New-Object System.Drawing.Point(12, 60)
$button.Size = New-Object System.Drawing.Size(200, 32)
$button.Text = "フリーズ開始"

$textBox = New-Object System.Windows.Forms.TextBox
$textBox.Location = New-Object System.Drawing.Point(12, 104)
$textBox.Size = New-Object System.Drawing.Size(480, 160)
$textBox.Multiline = $true
$textBox.Font = New-Object System.Drawing.Font("Meiryo UI", 11)
$textBox.Text = "ここで IME の状態を確認できます"

$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 1000
$script:remaining = 0

$timer.Add_Tick({
    $script:remaining--
    if ($script:remaining -gt 0) {
        $status.Text = "フリーズまで $($script:remaining) 秒..."
        return
    }

    $timer.Stop()
    $frozenAt = Get-Date -Format "HH:mm:ss"
    $status.Text = "フリーズ中 ($frozenAt から ${FreezeSeconds} 秒)"
    $status.Refresh()

    # UI スレッドを止める = メッセージを一切処理しない「応答なし」状態
    [System.Threading.Thread]::Sleep($FreezeSeconds * 1000)

    $recoveredAt = Get-Date -Format "HH:mm:ss"
    $status.Text = "復帰しました ($frozenAt - $recoveredAt)"
    $button.Enabled = $true
    $textBox.Focus() | Out-Null
})

$button.Add_Click({
    $button.Enabled = $false
    $script:remaining = $DelaySeconds
    $status.Text = "フリーズまで $DelaySeconds 秒..."
    $textBox.Focus() | Out-Null
    $timer.Start()
})

$form.Controls.AddRange(@($status, $button, $textBox))
[void]$form.ShowDialog()
