#!/bin/sh
# Creates a self-signed code-signing identity in your login keychain, for
# scripts/bundle.sh to sign Poom3D with. Run it once.
#
# With an ad-hoc signature every rebuild is a different app to macOS, which
# asks for the System Audio Recording permission again each time. Signed with
# the same identity, rebuilds stay the same app.
#
# macOS asks for your login password twice: once here, to trust the
# certificate for code signing, and once the first time bundle.sh signs with
# it, where "Always Allow" keeps it from asking again.
set -eu

name="Poom3D Local Signing"
keychain="$HOME/Library/Keychains/login.keychain-db"

if security find-identity -v -p codesigning | grep -q "\"$name\""; then
	echo "\"$name\" is already there and valid."
	exit 0
fi

work=$(mktemp -d)
trap 'rm -f "$work"/*; rmdir "$work"' EXIT

cat > "$work/cert.conf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $name
[ext]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
EOF

# The system's LibreSSL: its PKCS#12 files are the kind `security` reads
openssl=/usr/bin/openssl
"$openssl" req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$work/cert.conf" \
	-keyout "$work/key.pem" -out "$work/cert.pem" 2>/dev/null
# Only protects the file between the next two commands
pass=$("$openssl" rand -hex 16)
"$openssl" pkcs12 -export -inkey "$work/key.pem" -in "$work/cert.pem" -name "$name" \
	-out "$work/identity.p12" -passout "pass:$pass"

security import "$work/identity.p12" -k "$keychain" -P "$pass" -T /usr/bin/codesign
# For this user only, and for code signing only
security add-trusted-cert -r trustRoot -p codeSign -k "$keychain" "$work/cert.pem"

if security find-identity -v -p codesigning | grep -q "\"$name\""; then
	echo "Created \"$name\". scripts/bundle.sh signs with it from now on."
else
	echo "\"$name\" was imported but is not valid for code signing." >&2
	echo "In Keychain Access, open the certificate and set Code Signing to Always Trust." >&2
	exit 1
fi
