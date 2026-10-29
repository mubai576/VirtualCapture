// vcam_verify.cpp — P4-4 J1+J2 DShow 验收：枚举 + IAMStreamConfig 格式检查
// 编译：cmake -S . -B build && cmake --build build --config Release
// 用法：vc-vcam-verify.exe
// 退出码：0=PASS  1=FAIL
#include <windows.h>
#include <dshow.h>
#include <strmif.h>
#include <ks.h>
#include <ksmedia.h>
#include <string>
#include <cstdio>
#pragma comment(lib,"strmiids.lib")
#pragma comment(lib,"ole32.lib")
#pragma comment(lib,"oleaut32.lib")

// NV12 subtype GUID: {3231564E-0000-0010-8000-00AA00389B71}
static const GUID MEDIASUBTYPE_NV12_EXPECTED =
    {0x3231564E,0x0000,0x0010,{0x80,0x00,0x00,0xAA,0x00,0x38,0x9B,0x71}};

static void FreeMediaType(AM_MEDIA_TYPE* pmt) {
    if (!pmt) return;
    if (pmt->cbFormat && pmt->pbFormat) CoTaskMemFree(pmt->pbFormat);
    if (pmt->pUnk) pmt->pUnk->Release();
    CoTaskMemFree(pmt);
}

int main() {
    printf("P4-4 J1+J2: DShow vcam enumerate + format check\n");
    printf("================================================\n");

    CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);

    ICreateDevEnum* pSDE = nullptr;
    HRESULT hr = CoCreateInstance(CLSID_SystemDeviceEnum, nullptr, CLSCTX_INPROC_SERVER,
                                   IID_ICreateDevEnum, (void**)&pSDE);
    if (FAILED(hr)) { printf("FAIL: CoCreate SystemDevEnum hr=0x%08X\n", (unsigned)hr); return 1; }

    IEnumMoniker* pEnum = nullptr;
    hr = pSDE->CreateClassEnumerator(CLSID_VideoInputDeviceCategory, &pEnum, 0);
    pSDE->Release();
    if (FAILED(hr) || !pEnum) { printf("FAIL: CreateClassEnumerator hr=0x%08X\n", (unsigned)hr); return 1; }

    bool j1_pass = false, j2_pass = false;
    IMoniker* pMon = nullptr;
    while (pEnum->Next(1, &pMon, nullptr) == S_OK) {
        // Get FriendlyName via IPropertyBag
        IPropertyBag* pBag = nullptr;
        hr = pMon->BindToStorage(nullptr, nullptr, IID_IPropertyBag, (void**)&pBag);
        std::wstring name;
        if (SUCCEEDED(hr) && pBag) {
            VARIANT var; VariantInit(&var);
            if (SUCCEEDED(pBag->Read(L"FriendlyName", &var, nullptr)) && var.vt == VT_BSTR && var.bstrVal)
                name = var.bstrVal;
            VariantClear(&var);
            pBag->Release();
        }
        // narrow for printf
        std::string narrow(name.begin(), name.end());
        printf("  device: %s\n", narrow.c_str());

        if (name.find(L"VirtualCapture") != std::wstring::npos) {
            printf("  J1 PASS: found 'VirtualCapture'\n");
            j1_pass = true;

            // Bind to filter
            IBaseFilter* pFilter = nullptr;
            hr = pMon->BindToObject(nullptr, nullptr, IID_IBaseFilter, (void**)&pFilter);
            if (FAILED(hr) || !pFilter) {
                printf("  FAIL BindToObject hr=0x%08X\n", (unsigned)hr);
                pMon->Release(); continue;
            }

            // Enumerate pins, find output pin with IAMStreamConfig
            IEnumPins* pEnumPins = nullptr;
            pFilter->EnumPins(&pEnumPins);
            if (pEnumPins) {
                IPin* pPin = nullptr;
                while (pEnumPins->Next(1, &pPin, nullptr) == S_OK) {
                    PIN_DIRECTION dir;
                    if (SUCCEEDED(pPin->QueryDirection(&dir)) && dir == PINDIR_OUTPUT) {
                        IAMStreamConfig* pSC = nullptr;
                        if (SUCCEEDED(pPin->QueryInterface(IID_IAMStreamConfig, (void**)&pSC)) && pSC) {
                            AM_MEDIA_TYPE* pmt = nullptr;
                            hr = pSC->GetFormat(&pmt);
                            if (SUCCEEDED(hr) && pmt && pmt->pbFormat) {
                                // Parse VIDEOINFOHEADER
                                VIDEOINFOHEADER* vih = (VIDEOINFOHEADER*)pmt->pbFormat;
                                BITMAPINFOHEADER& bih = vih->bmiHeader;
                                REFERENCE_TIME avg = vih->AvgTimePerFrame;
                                DWORD w = (DWORD)abs(bih.biWidth);
                                DWORD h2 = (DWORD)abs(bih.biHeight);
                                DWORD fourcc = bih.biCompression;
                                char fcc[5] = {(char)(fourcc),(char)(fourcc>>8),(char)(fourcc>>16),(char)(fourcc>>24),0};
                                bool subtype_nv12 = (pmt->subtype.Data1 == MEDIASUBTYPE_NV12_EXPECTED.Data1);
                                printf("  IAMStreamConfig GetFormat:\n");
                                printf("    biWidth=%u biHeight=%u fourcc=%s subtype.Data1=0x%08X\n",
                                       w, h2, fcc, (unsigned)pmt->subtype.Data1);
                                printf("    AvgTimePerFrame=%lld (%.1f fps)\n",
                                       (long long)avg, avg ? 1e7/(double)avg : 0.0);
                                bool ok = (w == 640 && h2 == 640 &&
                                           (fourcc == 0x3231564EU || subtype_nv12) &&
                                           avg == 41666);
                                if (ok) { printf("  J2 PASS: NV12 640x640 AvgTimePerFrame=41666\n"); j2_pass = true; }
                                else      printf("  J2 FAIL: format mismatch\n");
                                FreeMediaType(pmt);
                            } else {
                                printf("  FAIL GetFormat hr=0x%08X\n", (unsigned)hr);
                            }
                            pSC->Release();
                        }
                    }
                    pPin->Release();
                    if (j2_pass) break;
                }
                pEnumPins->Release();
            }
            pFilter->Release();
        }
        pMon->Release();
        if (j1_pass) break;
    }
    pEnum->Release();
    CoUninitialize();

    printf("\n================================================\n");
    printf("J1 Enumerate: %s\n", j1_pass ? "PASS" : "FAIL");
    printf("J2 Format:    %s\n", j2_pass ? "PASS" : "FAIL");
    bool all_pass = j1_pass && j2_pass;
    printf("\nResult: %s\n", all_pass ? "PASS" : "FAIL");
    return all_pass ? 0 : 1;
}
