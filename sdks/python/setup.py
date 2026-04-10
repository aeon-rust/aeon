"""Setup for aeon-processor Python SDK."""

from setuptools import setup

setup(
    name="aeon-processor",
    version="0.1.0",
    description="Aeon Processor SDK for Python (Wasm + T3 WebTransport + T4 WebSocket)",
    py_modules=["aeon_processor", "aeon_transport"],
    python_requires=">=3.10",
    install_requires=[
        "websockets>=12.0",
        "msgpack>=1.0",
        "PyNaCl>=1.5",
    ],
    extras_require={
        "wasm": [],  # No extra deps for Wasm target (uses host bindings)
        "webtransport": ["aioquic>=1.2"],  # T3 WebTransport client
    },
    classifiers=[
        "Programming Language :: Python :: 3",
        "License :: OSI Approved :: Apache Software License",
        "Operating System :: OS Independent",
    ],
)
