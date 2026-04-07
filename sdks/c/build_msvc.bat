@echo off
setlocal

set MSVC_ROOT=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207
set WINSDK=C:\Program Files (x86)\Windows Kits\10
set WINSDK_VER=10.0.26100.0

set CL_EXE=%MSVC_ROOT%\bin\Hostx64\x64\cl.exe
set LINK_EXE=%MSVC_ROOT%\bin\Hostx64\x64\link.exe

set INCLUDE=%MSVC_ROOT%\include;%WINSDK%\Include\%WINSDK_VER%\ucrt;%WINSDK%\Include\%WINSDK_VER%\shared;%WINSDK%\Include\%WINSDK_VER%\um
set LIB=%MSVC_ROOT%\lib\x64;%WINSDK%\Lib\%WINSDK_VER%\ucrt\x64;%WINSDK%\Lib\%WINSDK_VER%\um\x64

if not exist build mkdir build

echo === Compiling aeon_processor.c ===
"%CL_EXE%" /nologo /W4 /c /std:c11 /Iinclude src\aeon_processor.c /Fo"build\aeon_processor.obj"
if errorlevel 1 goto :error

echo === Compiling test_aeon.c ===
"%CL_EXE%" /nologo /W4 /std:c11 /Iinclude test\test_aeon.c build\aeon_processor.obj /Fe"build\test_aeon.exe"
if errorlevel 1 goto :error

echo === Running tests ===
build\test_aeon.exe
goto :done

:error
echo BUILD FAILED
exit /b 1

:done
echo BUILD AND TESTS COMPLETE
