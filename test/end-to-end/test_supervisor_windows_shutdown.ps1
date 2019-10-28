hab pkg install core/nginx --channel stable
hab pkg install core/windows-service

Describe "Clean Habitat Shutdown" {
    Start-Service Habitat
    hab pkg install core/nginx
    Wait-Supervisor -Timeout 45
    hab svc load core/nginx
    Wait-SupervisorService nginx -Timeout 20
    It "Starts running nginx" {
        # This will error with a 403 because nginx is not running any sites
        try  { Invoke-WebRequest "http://localhost" -Method HEAD -UseBasicParsing }
        catch [System.Net.WebException] { $response = $_.Exception.Response }
        $response.Server | Should -BeLike "nginx/*"
    }
    It "Stops all processes" {
        Stop-Service Habitat
        Get-Process hab-sup -ErrorAction SilentlyContinue | Should -Be $null
        Get-Process hab-launch -ErrorAction SilentlyContinue | Should -Be $null
        Get-Process pwsh -ErrorAction SilentlyContinue | Should -Be $null
        Get-Process nginx -ErrorAction SilentlyContinue | Should -Be $null
    }
}
