#!/usr/bin/env python3
import http.client
import sys
import time


def connect(port):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    connection.request("GET", "/events")
    return connection, connection.getresponse()


def main():
    port = int(sys.argv[1])
    clients = []
    try:
        for _ in range(16):
            connection, response = connect(port)
            assert response.status == 200, response.status
            clients.append((connection, response))
        rejected, response = connect(port)
        assert response.status == 503, response.status
        response.read()
        rejected.close()

        clients.pop(0)[0].close()
        deadline = time.monotonic() + 8
        while True:
            replacement, response = connect(port)
            if response.status == 200:
                replacement.close()
                break
            response.read()
            replacement.close()
            assert response.status == 503, response.status
            if time.monotonic() >= deadline:
                raise AssertionError("SSE slot was not released after disconnect")
            time.sleep(0.2)
    finally:
        for connection, _ in clients:
            connection.close()


if __name__ == "__main__":
    main()
