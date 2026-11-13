# Test vectors from Espressif's reference SRP6a client (esp_local_ctrl's srp6a.py),
# which works against the device's protocomm Security 2.
import hashlib, os, sys
sys.path.insert(0, os.path.join(os.environ["IDF_PATH"], "examples/protocols/esp_local_ctrl/scripts"))
sys.path.insert(0, os.path.join(os.environ["IDF_PATH"], "examples/protocols/esp_local_ctrl/scripts/security"))
import srp6a
from utils import long_to_bytes, bytes_to_long

user, password = 'sidp', 'correct horse battery staple'
salt = bytes.fromhex('8899aabbccddeeff0011223344556677')  # no leading zero: Python would drop it
assert hashlib.sha512(f'{user}:{password}'.encode()).digest()[0] != 0  # Python strips a leading zero
N, g = srp6a.get_ng(srp6a.NG_3072)
x = srp6a.calculate_x(hashlib.sha512, salt, user, password)
v = pow(g, x, N)

c = srp6a.Srp6a(user, password)
c.a = int('a1' * 32, 16)
c.A = pow(g, c.a, N)
assert len(long_to_bytes(c.A)) == 384
# A device's B with a fixed b: B = k v + g^b mod N
b = int('b2' * 32, 16)
B = (c.k * v + pow(g, b, N)) % N
M = c.process_challenge(salt, long_to_bytes(B))
hN, hg = hashlib.sha512(long_to_bytes(N)).digest(), hashlib.sha512(bytes(383) + bytes([g])).digest()
assert all(x != y for x, y in zip(hN, hg))  # a zero XOR byte would be dropped too
print('salt', salt.hex())
print('verifier', long_to_bytes(v).hex())
print('a', long_to_bytes(c.a).hex())
print('A', long_to_bytes(c.A).hex())
print('B', long_to_bytes(B).hex())
print('M', M.hex())
print('H_AMK', c.H_AMK.hex())
