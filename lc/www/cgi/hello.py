#!/usr/bin/env python3
"""A minimal CGI script: echoes request metadata and the request body."""
import os
import sys

body = sys.stdin.buffer.read()

print("Content-Type: text/plain; charset=utf-8")
print("X-CGI-Demo: 1")
print()  # blank line ends CGI headers
print("Hello from CGI")
print("REQUEST_METHOD:", os.environ.get("REQUEST_METHOD", ""))
print("QUERY_STRING:", os.environ.get("QUERY_STRING", ""))
print("CONTENT_LENGTH:", os.environ.get("CONTENT_LENGTH", ""))
print("PATH_INFO:", os.environ.get("PATH_INFO", ""))
print("CWD:", os.getcwd())
print("BODY_LEN:", len(body))
print("BODY:", body.decode("utf-8", "replace"))
