"""Minimal USB/IP client to exercise the spike server without a driver."""
import socket
import struct
import time


def recv_exact(s, n):
    b = b''
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c:
            raise EOFError
        b += c
    return b


# Device list
s = socket.create_connection(('127.0.0.1', 3240))
s.sendall(struct.pack('>HHI', 0x0111, 0x8005, 0))
hdr = recv_exact(s, 12)
ver, code, status, ndev = struct.unpack('>HHII', hdr)
dev = recv_exact(s, 312)
busid = dev[256:288].rstrip(b'\0').decode()
busnum, devnum, speed, vid, pid, bcd = struct.unpack('>IIIHHH', dev[288:306])
nif = dev[311]
ifs = recv_exact(s, 4 * nif)
print(f'devlist: ver={ver:#x} code={code:#x} status={status} ndev={ndev} busid={busid} speed={speed} vid={vid:#06x} pid={pid:#06x} interfaces={nif} {ifs.hex()}')
s.close()

# Import
s = socket.create_connection(('127.0.0.1', 3240))
s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
s.sendall(struct.pack('>HHI', 0x0111, 0x8003, 0) + b'1-1'.ljust(32, b'\0'))
ver, code, status = struct.unpack('>HHI', recv_exact(s, 8))
recv_exact(s, 312)
print(f'import: code={code:#x} status={status}')
seq = [0]


def submit(ep, dir_in, length, setup=b'\0' * 8, data=b'', packets=None):
    seq[0] += 1
    np = len(packets) if packets is not None else 0xFFFFFFFF
    h = struct.pack('>IIIII', 1, seq[0], (1 << 16) | 2, 1 if dir_in else 0, ep)
    h += struct.pack('>IIIII', 0, length, 0, np, 1) + setup
    iso = b''
    if packets is not None:
        for off, ln in packets:
            iso += struct.pack('>IIII', off, ln, 0, 0)
    s.sendall(h + data + iso)
    return seq[0]


def read_ret():
    h = recv_exact(s, 48)
    cmd, sq = struct.unpack('>II', h[:8])
    if cmd == 4:
        return cmd, sq, struct.unpack('>i', h[20:24])[0], None, None
    status, actual, start, np, errs = struct.unpack('>iiiii', h[20:40])
    return cmd, sq, status, actual, np


def control(setup, length):
    submit(0, True, length, setup)
    cmd, sq, status, actual, np = read_ret()
    data = recv_exact(s, actual) if actual and status == 0 else b''
    return status, data


st, d = control(bytes([0x80, 6, 0, 1, 0, 0, 18, 0]), 18)
print('device descriptor', st, d.hex())
st, d = control(bytes([0x80, 6, 0, 2, 0, 0, 9, 0]), 9)
total = d[2] | d[3] << 8
st, d = control(bytes([0x80, 6, 0, 2, 0, 0, total & 0xff, total >> 8]), total)
print('config descriptor', st, len(d), 'bytes (wTotalLength', total, ')')
st, d = control(bytes([0x80, 6, 2, 3, 0x09, 0x04, 255, 0]), 255)
print('product string', st, d[2:].decode('utf-16-le'))
st, d = control(bytes([0x80, 6, 0, 6, 0, 0, 10, 0]), 10)
print('device qualifier (expect stall -32):', st)
st, d = control(bytes([0x00, 9, 1, 0, 0, 0, 0, 0]), 0)
print('SET_CONFIGURATION', st)
st, d = control(bytes([0x01, 11, 1, 0, 1, 0, 0, 0]), 0)
st, d = control(bytes([0x01, 11, 1, 0, 2, 0, 0, 0]), 0)
print('SET_INTERFACE 1/1 and 2/1', st)

# Iso: OUT 10 x 1 ms packets of a ramp, then IN 10 x 1 ms.
pk = 192
out_data = b''.join(struct.pack('<hh', i, i) for i in range(480))
t0 = time.perf_counter()
submit(1, False, len(out_data), data=out_data, packets=[(i * pk, pk) for i in range(10)])
submit(1, False, len(out_data), data=out_data, packets=[(i * pk, pk) for i in range(10)])
times = []
for _ in range(2):
    cmd, sq, status, actual, np = read_ret()
    recv_exact(s, np * 16)
    times.append((time.perf_counter() - t0) * 1000)
    print(f'OUT ret seq={sq} status={status} actual={actual} packets={np} at {times[-1]:.1f} ms')
t0 = time.perf_counter()
submit(2, True, 10 * 196, packets=[(i * 196, 196) for i in range(10)])
cmd, sq, status, actual, np = read_ret()
payload = recv_exact(s, actual)
iso = recv_exact(s, np * 16)
first = struct.unpack('<h', payload[:2])[0]
print(f'IN ret seq={sq} status={status} actual={actual} packets={np} at {(time.perf_counter()-t0)*1000:.1f} ms; first sample {first}; packet0 actual={struct.unpack(">I", iso[8:12])[0]}')

# Unlink a pending IN URB.
target = submit(2, True, 10 * 196, packets=[(i * 196, 196) for i in range(10)])
seq[0] += 1
s.sendall(struct.pack('>IIIII', 2, seq[0], (1 << 16) | 2, 0, 2) + struct.pack('>I', target) + b'\0' * 24)
cmd, sq, status, _, _ = read_ret()
print(f'unlink reply cmd={cmd} seq={sq} status={status} (expect 4, -104)')
s.settimeout(0.1)
try:
    s.recv(1)
    print('ERROR: got data after unlink')
except socket.timeout:
    print('no completion for the unlinked URB: ok')
s.close()
