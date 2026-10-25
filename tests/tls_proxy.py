"""TLS in front of a plain-HTTP omaforge, for testing clients that insist on HTTPS (gh).

    python3 tests/tls_proxy.py <listen-port> <backend-port> <cert.pem> <key.pem>
"""
import asyncio
import ssl
import sys


async def pipe(reader, writer):
    try:
        while data := await reader.read(65536):
            writer.write(data)
            await writer.drain()
    except (ConnectionError, asyncio.IncompleteReadError):
        pass
    finally:
        writer.close()


async def handle(client_reader, client_writer, backend_port):
    try:
        backend_reader, backend_writer = await asyncio.open_connection("127.0.0.1", backend_port)
    except OSError:
        client_writer.close()
        return
    await asyncio.gather(pipe(client_reader, backend_writer), pipe(backend_reader, client_writer))


async def main():
    listen, backend, cert, key = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3], sys.argv[4]
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    server = await asyncio.start_server(
        lambda r, w: handle(r, w, backend), "127.0.0.1", listen, ssl=context
    )
    async with server:
        await server.serve_forever()


asyncio.run(main())
