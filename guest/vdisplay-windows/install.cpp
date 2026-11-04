#define UNICODE
#define _UNICODE
#include <windows.h>
#include <setupapi.h>
#include <newdev.h>
#include <stdio.h>

static const GUID display_class = {0x4d36e968, 0xe325, 0x11ce, {0xbf, 0xc1, 0x08, 0x00, 0x2b, 0xe1, 0x03, 0x18}};
static const wchar_t hardware_id[] = L"Root\\VMLABDISPLAY\0";

static void activation_error(const wchar_t* operation, DWORD error) {
    HANDLE source = RegisterEventSourceW(nullptr, L"VMLabDisplay");
    if (!source) return;
    wchar_t message[256];
    DWORD session = 0;
    ProcessIdToSessionId(GetCurrentProcessId(), &session);
    swprintf_s(message, L"%s failed: Win32 %lu, session %lu", operation, error, session);
    const wchar_t* strings[] = {message};
    ReportEventW(source, EVENTLOG_ERROR_TYPE, 0, 2, nullptr, 1, 0, strings, nullptr);
    DeregisterEventSource(source);
}

static DWORD activate_display() {
    HDESK previous = GetThreadDesktop(GetCurrentThreadId());
    HDESK input = OpenInputDesktop(0, FALSE, GENERIC_ALL);
    if (!input) { DWORD error = GetLastError(); activation_error(L"OpenInputDesktop", error); return error; }
    if (!SetThreadDesktop(input)) {
        DWORD error = GetLastError(); CloseDesktop(input); activation_error(L"SetThreadDesktop", error); return error;
    }
    struct RestoreDesktop {
        HDESK previous;
        HDESK input;
        ~RestoreDesktop() { SetThreadDesktop(previous); CloseDesktop(input); }
    } restore = {previous, input};
    UINT32 path_count = 0, mode_count = 0;
    LONG result = GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &path_count, &mode_count);
    if (result != ERROR_SUCCESS) { activation_error(L"GetDisplayConfigBufferSizes", DWORD(result)); return DWORD(result); }
    auto paths = static_cast<DISPLAYCONFIG_PATH_INFO*>(HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY,
        SIZE_T(path_count) * sizeof(DISPLAYCONFIG_PATH_INFO)));
    auto modes = static_cast<DISPLAYCONFIG_MODE_INFO*>(HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY,
        SIZE_T(mode_count) * sizeof(DISPLAYCONFIG_MODE_INFO)));
    if (!paths || !modes) {
        if (paths) HeapFree(GetProcessHeap(), 0, paths);
        if (modes) HeapFree(GetProcessHeap(), 0, modes);
        return ERROR_NOT_ENOUGH_MEMORY;
    }
    result = QueryDisplayConfig(QDC_ALL_PATHS, &path_count, paths, &mode_count, modes, nullptr);
    DWORD answer = ERROR_NOT_FOUND;
    if (result == ERROR_SUCCESS) {
        for (UINT32 i = 0; i < path_count; ++i) {
            DISPLAYCONFIG_TARGET_DEVICE_NAME name = {};
            name.header.type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            name.header.size = sizeof(name);
            name.header.adapterId = paths[i].targetInfo.adapterId;
            name.header.id = paths[i].targetInfo.id;
            if (DisplayConfigGetDeviceInfo(&name.header) != ERROR_SUCCESS ||
                _wcsicmp(name.monitorFriendlyDeviceName, L"VMLab display") != 0) continue;
            DISPLAYCONFIG_TARGET_PREFERRED_MODE preferred = {};
            preferred.header.type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_PREFERRED_MODE;
            preferred.header.size = sizeof(preferred);
            preferred.header.adapterId = paths[i].targetInfo.adapterId;
            preferred.header.id = paths[i].targetInfo.id;
            answer = DWORD(DisplayConfigGetDeviceInfo(&preferred.header));
            if (answer) break;
            bool sole_active = (paths[i].flags & DISPLAYCONFIG_PATH_ACTIVE) != 0;
            for (UINT32 j = 0; j < path_count; ++j) {
                if (j != i && (paths[j].flags & DISPLAYCONFIG_PATH_ACTIVE)) sole_active = false;
            }
            if (sole_active && paths[i].sourceInfo.modeInfoIdx < mode_count) {
                const auto& current = modes[paths[i].sourceInfo.modeInfoIdx].sourceMode;
                if (current.width == preferred.width && current.height == preferred.height &&
                    current.position.x == 0 && current.position.y == 0) break;
            }
            DISPLAYCONFIG_PATH_INFO path = paths[i];
            DISPLAYCONFIG_MODE_INFO mode = {};
            mode.infoType = DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE;
            mode.id = path.sourceInfo.id;
            mode.adapterId = path.sourceInfo.adapterId;
            mode.sourceMode.width = preferred.width;
            mode.sourceMode.height = preferred.height;
            mode.sourceMode.position = {0, 0};
            mode.sourceMode.pixelFormat = DISPLAYCONFIG_PIXELFORMAT_32BPP;
            path.flags = DISPLAYCONFIG_PATH_ACTIVE;
            path.sourceInfo.modeInfoIdx = 0;
            path.targetInfo.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
            path.targetInfo.rotation = DISPLAYCONFIG_ROTATION_IDENTITY;
            path.targetInfo.scaling = DISPLAYCONFIG_SCALING_IDENTITY;
            path.targetInfo.refreshRate = {60, 1};
            // Persist one console path so Windows also places sign-in on the indirect monitor.
            answer = DWORD(SetDisplayConfig(1, &path, 1, &mode, SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG |
                SDC_SAVE_TO_DATABASE | SDC_ALLOW_CHANGES));
            if (answer) activation_error(L"SetDisplayConfig", answer);
            break;
        }
    } else answer = DWORD(result);
    HeapFree(GetProcessHeap(), 0, paths);
    HeapFree(GetProcessHeap(), 0, modes);
    return answer;
}

static DWORD start_console(const wchar_t* arguments, HANDLE& child) {
    DWORD session = WTSGetActiveConsoleSessionId();
    if (session == 0xffffffff) return ERROR_NO_SUCH_LOGON_SESSION;
    HANDLE source = nullptr, token = nullptr;
    if (!OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &source)) {
        DWORD error = GetLastError(); fputs("Opening the installer process token failed.\n", stderr); return error;
    }
    BOOL duplicated = DuplicateTokenEx(source, MAXIMUM_ALLOWED, nullptr, SecurityImpersonation, TokenPrimary, &token);
    DWORD error = duplicated ? ERROR_SUCCESS : GetLastError();
    CloseHandle(source);
    if (!duplicated) { fputs("Duplicating the installer token failed.\n", stderr); return error; }
    if (!SetTokenInformation(token, TokenSessionId, &session, sizeof(session))) {
        error = GetLastError(); fputs("Assigning the console session to the token failed.\n", stderr); CloseHandle(token); return error;
    }
    wchar_t image[32768], command[32768];
    if (!GetModuleFileNameW(nullptr, image, ARRAYSIZE(image))) { error = GetLastError(); CloseHandle(token); return error; }
    if (swprintf_s(command, L"\"%s\" %s", image, arguments) < 0) { CloseHandle(token); return ERROR_BUFFER_OVERFLOW; }
    STARTUPINFOW startup = {};
    startup.cb = sizeof(startup);
    wchar_t desktop[] = L"winsta0\\default";
    startup.lpDesktop = desktop;
    PROCESS_INFORMATION process = {};
    BOOL created = CreateProcessAsUserW(token, image, command, nullptr, nullptr, FALSE,
        CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT, nullptr, nullptr, &startup, &process);
    error = created ? ERROR_SUCCESS : GetLastError();
    CloseHandle(token);
    if (!created) { fputs("Starting the console activation process failed.\n", stderr); return error; }
    CloseHandle(process.hThread);
    child = process.hProcess;
    return ERROR_SUCCESS;
}

static DWORD activate_console() {
    HANDLE child = nullptr;
    DWORD error = start_console(L"--activate", child);
    if (error) return error;
    DWORD waited = WaitForSingleObject(child, 30000);
    if (waited == WAIT_OBJECT_0) GetExitCodeProcess(child, &error);
    else error = ERROR_TIMEOUT;
    CloseHandle(child);
    if (error) fputs("The console display configuration call failed.\n", stderr);
    return error;
}

static HANDLE service_stop;
static SERVICE_STATUS_HANDLE service_handle;
static SERVICE_STATUS service_status = {SERVICE_WIN32_OWN_PROCESS, SERVICE_START_PENDING, 0, 0, 0, 0, 0};

static void WINAPI service_control(DWORD control) {
    if (control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN) {
        service_status.dwCurrentState = SERVICE_STOP_PENDING;
        service_status.dwControlsAccepted = 0;
        SetServiceStatus(service_handle, &service_status);
        SetEvent(service_stop);
    }
}

static void WINAPI service_main(DWORD, wchar_t**) {
    service_handle = RegisterServiceCtrlHandlerW(L"VMLabDisplayConsole", service_control);
    if (!service_handle) return;
    wchar_t event_name[96];
    swprintf_s(event_name, L"Global\\VMLabDisplayStop-%lu", GetCurrentProcessId());
    service_stop = CreateEventW(nullptr, TRUE, FALSE, event_name);
    if (!service_stop) {
        service_status.dwCurrentState = SERVICE_STOPPED;
        service_status.dwWin32ExitCode = GetLastError();
        SetServiceStatus(service_handle, &service_status);
        return;
    }
    service_status.dwCurrentState = SERVICE_RUNNING;
    service_status.dwControlsAccepted = SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN;
    SetServiceStatus(service_handle, &service_status);
    HANDLE child = nullptr;
    while (WaitForSingleObject(service_stop, 1000) == WAIT_TIMEOUT) {
        if (child && WaitForSingleObject(child, 0) == WAIT_OBJECT_0) { CloseHandle(child); child = nullptr; }
        if (!child) {
            wchar_t arguments[128];
            swprintf_s(arguments, L"--watch %s", event_name);
            DWORD error = start_console(arguments, child);
            if (error != ERROR_NO_SUCH_LOGON_SESSION && error) activation_error(L"start_console", error);
        }
    }
    if (child) { WaitForSingleObject(child, 5000); CloseHandle(child); }
    CloseHandle(service_stop);
    service_status.dwCurrentState = SERVICE_STOPPED;
    service_status.dwControlsAccepted = 0;
    SetServiceStatus(service_handle, &service_status);
}

static DWORD watch_console(const wchar_t* event_name) {
    HANDLE stop = OpenEventW(SYNCHRONIZE, FALSE, event_name);
    if (!stop) return GetLastError();
    DWORD session = 0;
    ProcessIdToSessionId(GetCurrentProcessId(), &session);
    DWORD previous_error = ERROR_SUCCESS;
    do {
        if (WTSGetActiveConsoleSessionId() != session) break;
        DWORD error = activate_display();
        if (error && error != previous_error) activation_error(L"activate_display", error);
        previous_error = error;
    } while (WaitForSingleObject(stop, 1000) == WAIT_TIMEOUT);
    CloseHandle(stop);
    return ERROR_SUCCESS;
}

static bool existing_device() {
    HDEVINFO devices = SetupDiGetClassDevsW(&display_class, nullptr, nullptr, 0);
    if (devices == INVALID_HANDLE_VALUE) return false;
    bool found = false;
    SP_DEVINFO_DATA data = {};
    data.cbSize = sizeof(data);
    for (DWORD i = 0; SetupDiEnumDeviceInfo(devices, i, &data); ++i) {
        wchar_t ids[4096] = {};
        if (!SetupDiGetDeviceRegistryPropertyW(devices, &data, SPDRP_HARDWAREID, nullptr,
            reinterpret_cast<BYTE*>(ids), sizeof(ids), nullptr)) continue;
        for (const wchar_t* id = ids; *id; id += wcslen(id) + 1) {
            if (_wcsicmp(id, hardware_id) == 0) found = true;
        }
    }
    SetupDiDestroyDeviceInfoList(devices);
    return found;
}

int wmain(int argc, wchar_t** argv) {
    if (argc == 2 && wcscmp(argv[1], L"--service") == 0) {
        wchar_t name[] = L"VMLabDisplayConsole";
        SERVICE_TABLE_ENTRYW table[] = {{name, service_main}, {nullptr, nullptr}};
        return StartServiceCtrlDispatcherW(table) ? 0 : int(GetLastError());
    }
    if (argc == 3 && wcscmp(argv[1], L"--watch") == 0) return int(watch_console(argv[2]));
    if (argc == 2 && wcscmp(argv[1], L"--activate") == 0) return int(activate_display());
    if (argc == 2 && wcscmp(argv[1], L"--activate-console") == 0) {
        DWORD result = activate_console();
        if (result) fprintf(stderr, "Activating the indirect console display failed: %lu\n", result);
        return int(result);
    }
    if (argc != 3 || wcscmp(argv[1], L"--virtual-display-enabled") != 0) {
        fputs("Usage: install.exe --virtual-display-enabled <absolute INF path>\n", stderr);
        return 2;
    }
    wchar_t inf[32768];
    DWORD size = GetFullPathNameW(argv[2], ARRAYSIZE(inf), inf, nullptr);
    if (!size || size >= ARRAYSIZE(inf)) return 2;
    HDEVINFO created = INVALID_HANDLE_VALUE;
    SP_DEVINFO_DATA data = {};
    data.cbSize = sizeof(data);
    if (!existing_device()) {
        created = SetupDiCreateDeviceInfoList(&display_class, nullptr);
        if (created == INVALID_HANDLE_VALUE ||
            !SetupDiCreateDeviceInfoW(created, L"Display", &display_class, L"VMLab shared-memory display", nullptr,
                                      DICD_GENERATE_ID, &data) ||
            !SetupDiSetDeviceRegistryPropertyW(created, &data, SPDRP_HARDWAREID,
                                               reinterpret_cast<const BYTE*>(hardware_id), sizeof(hardware_id)) ||
            !SetupDiCallClassInstaller(DIF_REGISTERDEVICE, created, &data)) {
            DWORD error = GetLastError();
            if (created != INVALID_HANDLE_VALUE) SetupDiDestroyDeviceInfoList(created);
            fprintf(stderr, "Registering VMLab display failed: %lu\n", error);
            return 1;
        }
    }
    BOOL reboot = FALSE;
    BOOL installed = UpdateDriverForPlugAndPlayDevicesW(nullptr, hardware_id, inf, INSTALLFLAG_FORCE | INSTALLFLAG_NONINTERACTIVE, &reboot);
    DWORD error = installed ? ERROR_SUCCESS : GetLastError();
    if (created != INVALID_HANDLE_VALUE) {
        if (!installed) SetupDiCallClassInstaller(DIF_REMOVE, created, &data);
        SetupDiDestroyDeviceInfoList(created);
    }
    if (!installed) {
        fprintf(stderr, "Installing VMLab display failed: %lu\n", error);
        return 1;
    }
    puts(reboot ? "VMLab display installed; reboot required" : "VMLab display installed");
    return reboot ? 3010 : 0;
}
