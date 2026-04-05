"""Setup for aeon-processor Python SDK."""

from setuptools import setup

setup(
    name="aeon-processor",
    version="0.1.0",
    description="Aeon Processor SDK for Python (Wasm + T4 WebSocket transport)",
    py_modules=["aeon_processor", "aeon_transport"],
    python_requires=">=3.10",
    install_requires=[
        "websockets>=12.0",
        "msgpack>=1.0",
        "PyNaCl>=1.5",
    ],
    extras_require={
        "wasm": [],  # No extra deps for Wasm target (uses host bindings)
    },
    classifiers=[
        "Programming Language :: Python :: 3",
        "License :: OSI Approved :: Apache Software License",
        "Operating System :: OS Independent",
    ],
)
