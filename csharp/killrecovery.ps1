# killrecovery.ps1 — T6 手动验证：中途杀进程 → 重启恢复语义
#   已 flush 的 commit 文件完好 Active；Downloading 残留条目被清理；可重新整理。
#
# 用法（需要能跑 NativeBridge.Tests 的环境：dotnet SDK + native DLL 在探测路径上）：
#   pwsh csharp/killrecovery.ps1
#
# CI 不跑本脚本（杀整树进程的语义不适配 CI），T6 票上记录最后一次人工运行结果。
# 原理：stage1 测试下载两个快任务并 flush（耐久边界）后带一个慢任务挂住等死；
# 本脚本等 marker 出现后 taskkill /T /F 整树杀掉，再跑 stage2 验证重开语义。
$ErrorActionPreference = 'Stop'

$proj = Join-Path $PSScriptRoot 'NativeBridge.Tests'
$dir = Join-Path ([System.IO.Path]::GetTempPath()) 'nb-kill-recovery'
$marker = Join-Path $dir 'stage1.ready'
$env:NB_KILL_RECOVERY = '1'

Write-Host '== stage1: 下载 + flush 后挂住（模拟宿主运行中被杀）'
Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
$p = Start-Process dotnet -ArgumentList @('test', $proj, '--filter', 'FullyQualifiedName~KillRecovery_Stage1') -PassThru
$deadline = (Get-Date).AddMinutes(5)
while (-not (Test-Path $marker) -and (Get-Date) -lt $deadline -and -not $p.HasExited) {
    Start-Sleep -Milliseconds 500
}
if (-not (Test-Path $marker)) {
    Write-Error "stage1 未就绪（marker 未出现，进程退出=$($p.HasExited)）"
    if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
    exit 1
}

Write-Host '== 整树杀掉 stage1（taskkill /T /F）'
taskkill /T /F /PID $p.Id | Out-Null

Write-Host '== stage2: 重启进程重开 VFS，验证恢复语义'
dotnet test $proj --filter 'FullyQualifiedName~KillRecovery_Stage2'
if ($LASTEXITCODE -ne 0) {
    Write-Error 'stage2 验证失败'
    exit 1
}
Write-Host '== 通过：已 flush 的 commit 完好、Downloading 残留被清理、可重新整理'
