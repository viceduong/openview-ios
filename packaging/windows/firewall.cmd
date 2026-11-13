@echo off
rem Add or remove the Windows Firewall rules OpenView needs.
rem
rem   firewall.cmd add      allow inbound TCP for ovrelay.exe and UDP for ovhost.exe
rem   firewall.cmd remove   delete those rules again
rem
rem Must run as administrator. The rules are bound to the executables next
rem to this script and apply to Private and Domain networks only (switch a
rem network to Private in Windows Settings to use OpenView's LAN features
rem on it). The installer runs this script when the "Allow OpenView through
rem Windows Firewall" task is selected; portable users can run it by hand.
setlocal
set "DIR=%~dp0"
set "RELAY_RULE=OpenView relay (TCP-In)"
set "HOST_RULE=OpenView direct path (UDP-In)"

net session >nul 2>&1
if errorlevel 1 (
    echo firewall.cmd: run this from an elevated prompt ^(Run as administrator^).
    exit /b 1
)

if /i "%~1"=="add" goto add
if /i "%~1"=="remove" goto remove
echo usage: firewall.cmd add^|remove
exit /b 2

:add
call :delete_rules
netsh advfirewall firewall add rule name="%RELAY_RULE%" dir=in action=allow ^
    program="%DIR%ovrelay.exe" protocol=TCP profile=private,domain enable=yes ^
    description="OpenView relay: lets other devices on this network reach the relay running on this PC." >nul
if errorlevel 1 exit /b 1
netsh advfirewall firewall add rule name="%HOST_RULE%" dir=in action=allow ^
    program="%DIR%ovhost.exe" protocol=UDP profile=private,domain enable=yes ^
    description="OpenView host: direct UDP path (default port 4570), LAN sessions (4571) and mDNS discovery (5353)." >nul
if errorlevel 1 exit /b 1
echo OpenView firewall rules added.
exit /b 0

:remove
call :delete_rules
echo OpenView firewall rules removed.
exit /b 0

:delete_rules
netsh advfirewall firewall delete rule name="%RELAY_RULE%" >nul 2>&1
netsh advfirewall firewall delete rule name="%HOST_RULE%" >nul 2>&1
exit /b 0
