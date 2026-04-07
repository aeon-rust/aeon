cmake_minimum_required(VERSION 3.16)
project(aeon-processor-sdk C)
set(CMAKE_C_STANDARD 11)

add_library(aeon_processor STATIC src/aeon_processor.c)
target_include_directories(aeon_processor PUBLIC include)

add_executable(test_aeon test/test_aeon.c)
target_link_libraries(test_aeon aeon_processor)

enable_testing()
add_test(NAME aeon_tests COMMAND test_aeon)
