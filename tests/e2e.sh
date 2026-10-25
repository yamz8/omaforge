#!/usr/bin/env bash
# End-to-end test: runs a real omaforge server and drives it with git over HTTP and SSH.
#
#   cargo build && tests/e2e.sh [work-dir]
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${OMAFORGE_BIN:-$ROOT/target/debug/omaforge}
W=${1:-$(mktemp -d)}
rm -rf "$W" && mkdir -p "$W" && cd "$W" || exit 1
# Isolate git completely: a temporary HOME is not enough, because with XDG_CONFIG_HOME set
# `git config --global` writes to the real ~/.config/git/config.
unset XDG_CONFIG_HOME
export OMAFORGE_DATA=$W/data GIT_TERMINAL_PROMPT=0 GIT_CONFIG_NOSYSTEM=1 HOME=$W/home
export GIT_CONFIG_GLOBAL=$W/gitconfig
mkdir -p "$HOME"
git config --global user.name test
git config --global user.email test@example.com
git config --global init.defaultBranch main

PORT=${E2E_HTTP_PORT:-3917} SSH_PORT=${E2E_SSH_PORT:-3922}
URL=http://127.0.0.1:$PORT
SSH_URL=ssh://git@127.0.0.1:$SSH_PORT
pass=0 fail=0

check() {
    local name=$1; shift
    if "$@" >"$W/last.log" 2>&1; then
        echo "PASS $name"; pass=$((pass + 1))
    else
        echo "FAIL $name"; sed 's/^/    /' "$W/last.log" | tail -5; fail=$((fail + 1))
    fi
}
refuse() {
    local name=$1; shift
    if "$@" >"$W/last.log" 2>&1; then
        echo "FAIL $name (should have been refused)"; fail=$((fail + 1))
    else
        echo "PASS $name ($(grep -m1 -oE '(omaforge|fatal|[Ee]rror|.*Permission denied).*' "$W/last.log"))"
        pass=$((pass + 1))
    fi
}
start_server() {
    $BIN serve --listen 127.0.0.1:$PORT --ssh-listen 127.0.0.1:$SSH_PORT >>"$W/server.log" 2>&1 &
    SERVER=$!
    for _ in $(seq 100); do curl -sf $URL/health >/dev/null && return; sleep 0.1; done
    echo "server did not start"; cat "$W/server.log"; exit 1
}
# git over SSH as the owner of key $1.
ssh_as() { echo "ssh -i $W/$1 -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"; }

$BIN init 2>/dev/null
echo alicepassword | $BIN admin create-user alice --admin --password-stdin >/dev/null
echo bobpassword1 | $BIN admin create-user bob --password-stdin >/dev/null
TOKEN=$($BIN admin create-token alice laptop)
$BIN admin create-repo alice/hello >/dev/null
$BIN admin create-repo alice/secret --private >/dev/null
for k in alice bob mallory; do ssh-keygen -q -t ed25519 -N '' -C "$k@test" -f "$W/$k"; done
check "add alice's SSH key"          $BIN admin add-ssh-key alice laptop --key-file "$W/alice.pub"
check "add bob's SSH key from stdin" bash -c "$BIN admin add-ssh-key bob desktop < '$W/bob.pub'"
refuse "same key twice"              $BIN admin add-ssh-key bob again --key-file "$W/alice.pub"
refuse "garbage key"                 bash -c "echo 'ssh-ed25519 notakey' | $BIN admin add-ssh-key bob junk"

start_server
trap 'kill $SERVER 2>/dev/null' EXIT

echo "== HTTP"
git init -q src && cd src
for i in $(seq 200); do echo "line $i" >>file.txt; git add file.txt; git commit -qm "commit $i"; done
head -c 3000000 /dev/urandom >blob.bin && git add blob.bin && git commit -qm blob && git tag v1
check "push with token"            git push -q http://alice:$TOKEN@127.0.0.1:$PORT/alice/hello.git main v1
refuse "anonymous push"            git push -q $URL/alice/hello.git main:other
refuse "bob push to alice/hello"   git push -q http://bob:bobpassword1@127.0.0.1:$PORT/alice/hello.git main:other
refuse "wrong password"            git push -q http://alice:nope@127.0.0.1:$PORT/alice/hello.git main:other
check "push private w/ password"   git push -q http://alice:alicepassword@127.0.0.1:$PORT/alice/secret.git main
cd ..
check "anon clone v2"              git -c protocol.version=2 clone -q $URL/alice/hello.git c-v2
check "clone content matches"      bash -c "cmp src/blob.bin c-v2/blob.bin && [ \$(git -C c-v2 rev-list --count HEAD) = 201 ] && git -C c-v2 tag | grep -q v1"
check "anon clone v0"              git -c protocol.version=0 clone -q $URL/alice/hello c-v0
check "clone without .git suffix"  test -f c-v0/blob.bin
refuse "anon clone private"        git clone -q $URL/alice/secret.git p1
refuse "bob clone private"         git clone -q http://bob:bobpassword1@127.0.0.1:$PORT/alice/secret.git p2
check "alice clone private"        git clone -q http://alice:$TOKEN@127.0.0.1:$PORT/alice/secret.git p3
refuse "clone missing repo"        git clone -q $URL/alice/nope.git p4
(cd src && for i in $(seq 300); do echo "more $i" >>file.txt; git commit -qam "more $i"; done)
check "push incremental"           git -C src push -q http://alice:$TOKEN@127.0.0.1:$PORT/alice/hello.git main
check "fetch incremental v0"       git -C c-v0 -c protocol.version=0 pull -q --ff-only
check "fetched 501 commits"        test "$(git -C c-v0 rev-list --count HEAD)" = 501
check "path traversal 404"         bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' --path-as-is '$URL/../../etc/info/refs?service=git-upload-pack') != 200 ] && [ \$(curl -s -o /dev/null -w '%{http_code}' '$URL/alice/..%2F..%2Fetc/info/refs?service=git-upload-pack') = 404 ]"
check "dumb protocol refused"      bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' $URL/alice/hello.git/info/refs) = 403 ]"
# Local history the server lacks makes negotiation requests big enough (>1KB) for git to
# gzip. Protocol v2 negotiates in smaller rounds that stay under that, so use v0 here.
git clone -q $URL/alice/hello.git gz && (cd gz && git checkout -qb local && for i in $(seq 200); do echo "l $i" >>local.txt; git add local.txt; git commit -qm "local $i"; done)
(cd src && echo next >>file.txt && git commit -qam next && git push -q http://alice:$TOKEN@127.0.0.1:$PORT/alice/hello.git main)
check "gzipped fetch request"      bash -c "GIT_TRACE_CURL=1 GIT_TRACE_CURL_NO_DATA=1 git -C gz -c protocol.version=0 fetch -q origin 2>&1 | grep -qi 'Content-Encoding: gzip' && [ \$(git -C gz rev-parse origin/main) = \$(git -C src rev-parse main) ]"
check "token last_used recorded"   bash -c "sqlite3 $OMAFORGE_DATA/omaforge.db 'select last_used_at from tokens' | grep -q 20"

echo "== SSH"
export GIT_SSH_COMMAND
GIT_SSH_COMMAND=$(ssh_as alice)
check "ssh clone v2"               git -c protocol.version=2 clone -q $SSH_URL/alice/hello.git s-v2
check "ssh used protocol v2"       bash -c "GIT_TRACE_PACKET=1 git -c protocol.version=2 ls-remote $SSH_URL/alice/hello.git 2>&1 | grep -q '< version 2'"
check "ssh clone v0"               git -c protocol.version=0 clone -q $SSH_URL/alice/hello s-v0
check "ssh clone content matches"  bash -c "[ \$(git -C s-v2 rev-list --count HEAD) = 502 ] && cmp src/blob.bin s-v0/blob.bin"
(cd s-v2 && echo ssh >ssh.txt && git add ssh.txt && git commit -qm "via ssh")
check "ssh push"                   git -C s-v2 push -q origin main
check "ssh push visible over http" bash -c "git -C c-v2 pull -q --ff-only && test -f c-v2/ssh.txt"
check "ssh clone private (owner)"  git clone -q $SSH_URL/alice/secret.git s-secret
check "ssh key last_used recorded" bash -c "sqlite3 $OMAFORGE_DATA/omaforge.db \"select last_used_at from ssh_keys where name='laptop'\" | grep -q 20"
GIT_SSH_COMMAND=$(ssh_as bob)
check "bob ssh clone public"       git clone -q $SSH_URL/alice/hello.git s-bob
refuse "bob ssh push"              git -C s-bob push -q origin main:bob-branch
refuse "bob ssh clone private"     git clone -q $SSH_URL/alice/secret.git s-bob-secret
refuse "ssh missing repo"          git clone -q $SSH_URL/alice/nope.git s-nope
refuse "ssh traversal"             git clone -q "$SSH_URL/../../etc/passwd" s-trav
GIT_SSH_COMMAND=$(ssh_as mallory)
refuse "unknown key"               git clone -q $SSH_URL/alice/hello.git s-mallory
unset GIT_SSH_COMMAND
check "shell greets, exits 1"      bash -c "$(ssh_as alice) -p $SSH_PORT git@127.0.0.1 2>&1 </dev/null; [ \$? = 1 ]" 
check "shell greeting text"        bash -c "$(ssh_as alice) -p $SSH_PORT git@127.0.0.1 2>&1 </dev/null | grep -q 'Hi alice!'"
refuse "arbitrary command"         $(ssh_as alice) -p $SSH_PORT git@127.0.0.1 "cat /etc/passwd"
check "host key is private"        bash -c "[ \$(stat -c %a $OMAFORGE_DATA/ssh_host_ed25519_key) = 600 ]"
FP1=$(ssh-keyscan -p $SSH_PORT 127.0.0.1 2>/dev/null | ssh-keygen -lf - | awk '{print $2}')
kill $SERVER; wait $SERVER 2>/dev/null
start_server
FP2=$(ssh-keyscan -p $SSH_PORT 127.0.0.1 2>/dev/null | ssh-keygen -lf - | awk '{print $2}')
check "host key stable on restart" bash -c "[ -n '$FP1' ] && [ '$FP1' = '$FP2' ]"

echo "== Web"
page() { curl -s "$URL$1"; }
status() { curl -s -o /dev/null -w '%{http_code}' "$URL$1"; }
(cd src && git pull -q --ff-only $URL/alice/hello.git main && printf '\000\001binary' >small.bin && printf '# Hello\n\n![logo](img/logo.png) [notes](docs/notes.md)\n\n<script>alert(1)</script>\n\n```rust\nfn main() {}\n```\n' >README.md \
    && git add README.md small.bin && git commit -qm "Add README" && git push -q http://alice:$TOKEN@127.0.0.1:$PORT/alice/hello.git main)
check "index lists public repo"     bash -c "curl -s $URL/ | grep -q 'alice/hello' && ! curl -s $URL/ | grep -q 'alice/secret'"
check "repo home renders README"    bash -c "curl -s $URL/alice/hello | grep -q '<h1 id=\"user-content-hello\"'"
check "README script stripped"      bash -c "! curl -s $URL/alice/hello | grep -q '<script>alert'"
check "README links rewritten"      bash -c "curl -s $URL/alice/hello | grep -q 'href=\"/alice/hello/blob/main/docs/notes.md\"' && curl -s $URL/alice/hello | grep -q 'src=\"/alice/hello/raw/main/img/logo.png\"'"
check "README code highlighted"     bash -c "curl -s $URL/alice/hello | grep -q 'class=\"hl-'"
check "private repo hidden on web"  test "$(status /alice/secret)" = 404
check "file view"                   bash -c "curl -s $URL/alice/hello/blob/main/file.txt | grep -q 'line 200'"
check "raw is text/plain, sandboxed" bash -c "curl -sI $URL/alice/hello/raw/main/file.txt | grep -qi 'content-type: text/plain' && curl -sI $URL/alice/hello/raw/main/file.txt | grep -qi 'content-security-policy: .*sandbox'"
check "binary file not inlined"     bash -c "curl -s $URL/alice/hello/blob/main/small.bin | grep -q 'Binary file not shown'"
check "large file not inlined"      bash -c "curl -s $URL/alice/hello/blob/main/blob.bin | grep -q 'too large to display'"
check "history paginates"           bash -c "curl -s $URL/alice/hello/commits/main | grep -q 'page=2' && curl -s '$URL/alice/hello/commits/main?page=2' | grep -q 'Newer'"
check "file history"                bash -c "curl -s $URL/alice/hello/commits/main/blob.bin | grep -q '>blob<'"
check "commit diff"                 bash -c "curl -s $URL/alice/hello/commit/\$(git -C src rev-parse HEAD) | grep -q 'README.md'"
check "tag tree"                    test "$(status /alice/hello/tree/v1)" = 200
check "web path traversal 404"      test "$(status '/alice/hello/tree/main/..%2F..%2Fetc')" = 404
check "dir as blob redirects"       test "$(status /alice/hello/blob/main)" = 303
check "theme css served"            bash -c "curl -s $URL/assets/theme.css | grep -q -- '--accent:'"

echo "== Web sign-in"
A=$W/alice.jar B=$W/bob.jar A2=$W/alice2.jar
# csrf <jar> <path>: the CSRF token from a page's forms.
csrf() { curl -s -b "$1" "$URL$2" | grep -m1 -o 'name="csrf" value="[0-9a-f]*"' | grep -o '[0-9a-f]\{64\}'; }
# post <jar> <path> <form...>: POST a form, print "<status> <location>".
post() { local jar=$1 path=$2; shift 2; curl -s -o "$W/post.html" -w '%{http_code} %{redirect_url}' -b "$jar" -c "$jar" "${@/#/--data-urlencode=}" "$URL$path"; }
check "settings needs sign-in"      bash -c "[ \"\$(curl -s -o /dev/null -w '%{http_code} %{redirect_url}' $URL/settings/keys)\" = '303 $URL/login?next=/settings/keys' ]"
check "wrong password rejected"     bash -c "[ \"\$(curl -s -c $A --data 'login=alice&password=nope' -o $W/p.html -w '%{http_code}' $URL/login)\" = 200 ] && grep -q 'Incorrect username or password' $W/p.html"
check "unknown user rejected"       bash -c "curl -s --data 'login=nobody&password=whatever1' $URL/login | grep -q 'Incorrect username or password'"
check "sign in redirects to next"   bash -c "[ \"\$(curl -s -c $A --data 'login=alice&password=alicepassword&next=/alice/secret' -o /dev/null -w '%{redirect_url}' $URL/login)\" = '$URL/alice/secret' ]"
check "session cookie is HttpOnly"  bash -c "curl -s -D - -o /dev/null --data 'login=alice&password=alicepassword' $URL/login | grep -i '^set-cookie: omaforge_session=' | grep -i httponly | grep -qi 'samesite=lax'"
check "no open redirect"            bash -c "[ \"\$(curl -s --data 'login=alice&password=alicepassword&next=//evil.example' -o /dev/null -w '%{redirect_url}' $URL/login)\" = '$URL/' ]"
check "index shows own private repo" bash -c "curl -s -b $A $URL/ | grep -q 'alice/secret'"
check "owner sees private repo"     test "$(curl -s -o /dev/null -w '%{http_code}' -b $A $URL/alice/secret)" = 200
curl -s -c $B --data 'login=bob&password=bobpassword1' -o /dev/null $URL/login
check "other user can't see it"     bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -b $B $URL/alice/secret) = 404 ] && ! curl -s -b $B $URL/ | grep -q 'alice/secret'"
check "form without CSRF refused"   bash -c "[ \"\$(curl -s -o /dev/null -w '%{http_code}' -b $A --data 'csrf=0000&name=x' $URL/settings/tokens)\" = 403 ]"
check "bob's CSRF fails for alice"  bash -c "[ \"\$(curl -s -o /dev/null -w '%{http_code}' -b $A --data \"csrf=\$(csrf $B /settings/tokens)&name=x\" $URL/settings/tokens)\" = 403 ]"
WEB_TOKEN=$(curl -s -b $A --data "csrf=$(csrf $A /settings/tokens)&name=web" $URL/settings/tokens | grep -o 'omf_[0-9a-f]\{64\}')
check "token created on web"        test -n "$WEB_TOKEN"
check "web token clones private"    git clone -q http://alice:$WEB_TOKEN@127.0.0.1:$PORT/alice/secret.git web-secret
ssh-keygen -q -t ed25519 -N '' -C carol@test -f "$W/carol"
db() { sqlite3 "$OMAFORGE_DATA/omaforge.db" "$1"; }
# Let checks run as `bash -c` use the helpers too.
export W URL
export -f post csrf db
R=$(post $A /settings/keys csrf=$(csrf $A /settings/keys) name=carol "key=$(cat $W/carol.pub)")
check "SSH key added on web"        test "$R" = "303 $URL/settings/keys"
check "web key works over SSH"      env GIT_SSH_COMMAND="$(ssh_as carol)" git clone -q $SSH_URL/alice/secret.git web-ssh
post $A /settings/keys csrf=$(csrf $A /settings/keys) name=junk key=nonsense >/dev/null
check "bad SSH key shows an error"  grep -q 'look like an OpenSSH public key' $W/post.html
KEY_ID=$(db "select id from ssh_keys where name='carol'")
post $B /settings/keys/$KEY_ID/delete csrf=$(csrf $B /settings/keys) >/dev/null
check "bob can't delete alice's key" test "$(db "select count(*) from ssh_keys where id=$KEY_ID")" = 1
post $A /settings/keys/$KEY_ID/delete csrf=$(csrf $A /settings/keys) >/dev/null
check "alice deletes her key"       test "$(db "select count(*) from ssh_keys where id=$KEY_ID")" = 0
refuse "deleted key can't SSH"      env GIT_SSH_COMMAND="$(ssh_as carol)" git clone -q $SSH_URL/alice/secret.git web-ssh2
R=$(post $A /new csrf=$(csrf $A /new) name=made-on-web "description=From the browser" visibility=private)
check "create repo on web"          test "$R" = "303 $URL/alice/made-on-web"
check "new repo is private"         bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' $URL/alice/made-on-web) = 404 ] && curl -s -b $A $URL/alice/made-on-web | grep -q 'This repository is empty'"
check "push to web-made repo"       git -C src push -q http://alice:$WEB_TOKEN@127.0.0.1:$PORT/alice/made-on-web.git main
post $A /new csrf=$(csrf $A /new) name=made-on-web description= visibility=public >/dev/null
check "duplicate repo name refused" grep -q 'already exists' $W/post.html
post $A /new csrf=$(csrf $A /new) name=../x description= visibility=public >/dev/null
check "bad repo name refused"       grep -q 'Use letters, numbers' $W/post.html
curl -s -c $A2 --data 'login=alice&password=alicepassword' -o /dev/null $URL/login
post $A /settings/password csrf=$(csrf $A /settings/password) current=alicepassword new=newpassword9 confirm=newpassword9 >/dev/null
check "password change"             grep -q 'Password changed' $W/post.html
check "other sessions signed out"   bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -b $A2 $URL/alice/secret) = 404 ] && [ \$(curl -s -o /dev/null -w '%{http_code}' -b $A $URL/alice/secret) = 200 ]"
check "new password works for git"  git ls-remote -q http://alice:newpassword9@127.0.0.1:$PORT/alice/secret.git
refuse "old password doesn't"       git ls-remote -q http://alice:alicepassword@127.0.0.1:$PORT/alice/secret.git
post $A /logout csrf=$(csrf $A /settings/keys) >/dev/null
check "sign out"                    test "$(curl -s -o /dev/null -w '%{http_code}' -b $A $URL/alice/secret)" = 404
check "signed-out cookie is dead"   test "$(db "select count(*) from sessions s join users u on u.id = s.user_id where u.username = 'alice'")" = 0

echo "== Import local"
F=$W/fixtures && mkdir -p $F && (
    cd $F
    git init -q --bare upstream.git
    git clone -q upstream.git seed 2>/dev/null && cd seed && echo one >a && git add a && git commit -qm one \
        && git tag v2 && git checkout -qb feature/remote-only && echo two >b && git add b && git commit -qm two \
        && git push -q origin main feature/remote-only v2 && cd .. && rm -rf seed
    git clone -q upstream.git "clone a" && cd "clone a" && git checkout -qb wip && echo wip >w && git add w \
        && git commit -qm wip && git tag local-1 && echo stash >>a && git stash -q && echo dirty >>a \
        && mkdir sub && touch sub/x && git checkout -q main 2>/dev/null; cd $F
    git clone -q --bare upstream.git "Bare Project.git" && echo "Bare project from upstream" >"Bare Project.git/description"
    mkdir notgit && touch notgit/file
    git init -q emptyrepo
    git init -q -b master master-only && git -C master-only commit -q --allow-empty -m first
)
REFS_BEFORE=$(git -C "$F/clone a" for-each-ref --format='%(objectname) %(refname)' | sha256sum)
OUT=$($BIN import local "$F/clone a" "$F/Bare Project.git" "$F/notgit" "$F/clone a/sub" "$F/emptyrepo" "$F/master-only" --owner alice 2>&1)
echo "$OUT" | sed 's/^/    | /'
check "imports working tree"        grep -q 'imported .*clone a -> alice/clone-a (3 branches, 2 tags)' <<<"$OUT"
check "imports bare repo"           grep -q 'imported .*Bare Project.git -> alice/Bare-Project (2 branches, 1 tag)' <<<"$OUT"
check "warns about dirty tree"      grep -q 'has 2 uncommitted or untracked files (not imported)' <<<"$OUT"
check "warns about stash"           grep -q 'has 1 stash entry (not imported)' <<<"$OUT"
check "skips non-repo folder"       grep -q 'skipped .*notgit: not a git repository' <<<"$OUT"
check "skips folder inside repo"    grep -q 'skipped .*clone a/sub: not a git repository' <<<"$OUT"
check "skips repo without commits"  grep -q 'skipped .*emptyrepo: no commits' <<<"$OUT"
check "summary line"                grep -q '^3 imported, 3 skipped, 0 failed$' <<<"$OUT"
check "source repo untouched"       test "$(git -C "$F/clone a" for-each-ref --format='%(objectname) %(refname)' | sha256sum)" = "$REFS_BEFORE"
check "imported repos are private"  test "$(curl -s -o /dev/null -w '%{http_code}' $URL/alice/clone-a)" = 404
TOK=http://alice:$WEB_TOKEN@127.0.0.1:$PORT
check "origin-only branch imported" bash -c "git ls-remote $TOK/alice/clone-a.git | grep -q 'refs/heads/feature/remote-only$'"
check "local branch and tag too"    bash -c "git ls-remote $TOK/alice/clone-a.git | grep -q 'refs/heads/wip$' && git ls-remote $TOK/alice/clone-a.git | grep -q 'refs/tags/local-1$'"
check "content matches source"      bash -c "[ \"\$(git ls-remote $TOK/alice/clone-a.git refs/heads/wip | cut -f1)\" = \"\$(git -C '$F/clone a' rev-parse wip)\" ]"
check "default branch from source"  bash -c "git ls-remote --symref $TOK/alice/clone-a.git HEAD | grep -q 'ref: refs/heads/main'"
check "default branch master"       bash -c "git ls-remote --symref $TOK/alice/master-only.git HEAD | grep -q 'ref: refs/heads/master'"
check "description kept"            test "$(db "select description from repos where name='Bare-Project'")" = "Bare project from upstream"
OUT=$($BIN import local "$F/clone a" --owner alice 2>&1)
check "re-run skips existing"       grep -q 'skipped .*alice/clone-a already exists (use --update)' <<<"$OUT"
(cd "$F/clone a" && git checkout -q wip && echo more >>w && git commit -qam more)
OUT=$($BIN import local "$F/clone a" --owner alice --update 2>&1)
check "update fetches new commits"  bash -c "grep -q 'updated .*alice/clone-a' <<<'$OUT' && [ \"\$(git ls-remote $TOK/alice/clone-a.git refs/heads/wip | cut -f1)\" = \"\$(git -C '$F/clone a' rev-parse wip)\" ]"
SERVER_WIP=$(git ls-remote $TOK/alice/clone-a.git refs/heads/wip 2>/dev/null | cut -f1)
(cd "$F/clone a" && git commit -q --amend -m rewritten)
OUT=$($BIN import local "$F/clone a" --owner alice --update 2>&1)
check "update refuses rewrites"     grep -q 'failed .*history was rewritten, so these were left unchanged: wip' <<<"$OUT"
check "rewritten branch unchanged"  test "$(git ls-remote $TOK/alice/clone-a.git refs/heads/wip 2>/dev/null | cut -f1)" = "$SERVER_WIP"
git init -q -b main "$F/later" && git -C "$F/later" commit -q --allow-empty -m x
OUT=$($BIN import local "$F/later" --owner alice --dry-run 2>&1)
check "dry run reports the plan"   grep -q 'would import .*alice/later (1 branch, 0 tags)' <<<"$OUT"
check "dry run changes nothing"     bash -c "[ \"$(db "select count(*) from repos where name='later'")\" = 0 ] && [ ! -e $OMAFORGE_DATA/repos/alice/later.git ]"
$BIN import local "$F/later" --owner alice --public >/dev/null 2>&1
check "--public imports public"     git clone -q $URL/alice/later.git later-clone
refuse "unknown owner"              $BIN import local "$F/later" --owner nobody

echo "== Pull requests"
A=$W/alice-pr.jar B=$W/bob-pr.jar
curl -s -c $A --data 'login=alice&password=newpassword9' -o /dev/null $URL/login
curl -s -c $B --data 'login=bob&password=bobpassword1' -o /dev/null $URL/login
$BIN admin create-repo alice/prs >/dev/null
PRS=http://alice:$WEB_TOKEN@127.0.0.1:$PORT/alice/prs.git
git init -q -b main prs-src && (
    cd prs-src
    printf 'one\ntwo\nthree\n' >f.txt && git add f.txt && git commit -qm "Start"
    git checkout -qb feature && printf 'hello\n' >g.txt && git add g.txt && git commit -qm "Add g" \
        && printf 'hello world\n' >g.txt && git commit -qam "Improve g"
    git checkout -qb conflict main && sed -i 's/two/TWO/' f.txt && git commit -qam "Shout two"
    git checkout -qb squash-me main && echo s1 >s.txt && git add s.txt && git commit -qm "s1" && echo s2 >>s.txt && git commit -qam "s2"
    git checkout -qb rebase-me main && echo r1 >r.txt && git add r.txt && git commit -qm "Rebase one" && echo r2 >>r.txt && git commit -qam "Rebase two"
    git checkout -q main && sed -i 's/two/deux/' f.txt && git commit -qam "Translate two"
    git push -q $PRS main feature conflict squash-me rebase-me
)
# open_pr <jar> <head> <title>: open a pull request against main, print "<status> <location>".
open_pr() { post $1 /alice/prs/pulls csrf=$(csrf $1 "/alice/prs/compare?base=main&head=$2") base=main head=$2 "title=$3" "body=About $2"; }
pr_csrf() { csrf $1 /alice/prs/pull/$2; }
main_sha() { git ls-remote $PRS refs/heads/main 2>/dev/null | cut -f1; }
check "compare shows commits"       bash -c "curl -s '$URL/alice/prs/compare?base=main&head=feature' | grep -q 'Improve g'"
check "compare: no form signed out" bash -c "! curl -s '$URL/alice/prs/compare?base=main&head=feature' | grep -q 'Open pull request'"
check "compare: same branch"        bash -c "curl -s '$URL/alice/prs/compare?base=main&head=main' | grep -q 'Choose two different branches'"
check "bob opens a pull request"    test "$(open_pr $B feature 'Add g')" = "303 $URL/alice/prs/pull/1"
check "duplicate is refused"        bash -c "curl -s -b $B '$URL/alice/prs/compare?base=main&head=feature' | grep -q 'already an open pull request'"
check "listed with count"           bash -c "curl -s $URL/alice/prs/pulls | grep -q '#1 opened' && curl -s $URL/alice/prs | grep -q '<span class=\"count\">1</span>'"
check "conversation shows it"       bash -c "curl -s $URL/alice/prs/pull/1 | grep -q 'About feature'"
check "commits tab"                 bash -c "curl -s $URL/alice/prs/pull/1/commits | grep -q 'Add g' && ! curl -s $URL/alice/prs/pull/1/commits | grep -q 'Translate two'"
check "files tab is head's diff"    bash -c "curl -s $URL/alice/prs/pull/1/files | grep -q 'g.txt' && ! curl -s $URL/alice/prs/pull/1/files | grep -q 'f.txt'"
post $B /alice/prs/pull/1/comment csrf=$(pr_csrf $B 1) "body=Ready for **review**" >/dev/null
check "comment rendered"            bash -c "curl -s $URL/alice/prs/pull/1 | grep -q 'Ready for <strong>review</strong>'"
check "line link opens a form"      bash -c "curl -s -b $A '$URL/alice/prs/pull/1/files?path=g.txt&side=new&line=1' | grep -q 'Comment on line 1'"
HEAD1=$(git -C prs-src rev-parse feature)
post $A /alice/prs/pull/1/comment csrf=$(pr_csrf $A 1) "body=Nice line" path=g.txt side=new line=1 commit=$HEAD1 >/dev/null
check "line comment under its line" bash -c "curl -s $URL/alice/prs/pull/1/files | grep -A6 'hello world' | grep -q 'Nice line'"
check "line comment in timeline"    bash -c "curl -s $URL/alice/prs/pull/1 | grep -q 'g.txt:1'"
post $B /alice/prs/pull/1/review csrf=$(pr_csrf $B 1) state=approved >/dev/null
check "author can't self-approve"   grep -q "can&#39;t review your own pull request" $W/post.html
post $A /alice/prs/pull/1/review csrf=$(pr_csrf $A 1) state=approved "body=LGTM" >/dev/null
check "reviewer approves"           bash -c "curl -s $URL/alice/prs/pull/1 | grep -q 'approved these changes' && curl -s $URL/alice/prs/pull/1 | grep -q 'alice approved'"
check "bob can't merge"             test "$(post $B /alice/prs/pull/1/merge csrf=$(pr_csrf $B 1) method=merge | cut -d' ' -f1)" = 403
check "merge needs CSRF"            test "$(post $A /alice/prs/pull/1/merge csrf=nope method=merge | cut -d' ' -f1)" = 403
check "merge button for owner"      bash -c "curl -s -b $A $URL/alice/prs/pull/1 | grep -q 'Merge pull request'"
OLD_MAIN=$(main_sha)
check "alice merges"                test "$(post $A /alice/prs/pull/1/merge csrf=$(pr_csrf $A 1) method=merge)" = "303 $URL/alice/prs/pull/1"
check "merge commit has 2 parents"  bash -c "git -C prs-src fetch -q $PRS main && [ \$(git -C prs-src log -1 --format=%P FETCH_HEAD | wc -w) = 2 ] && [ \"\$(git -C prs-src log -1 --format=%P FETCH_HEAD)\" = '$OLD_MAIN $HEAD1' ]"
check "merge message"               bash -c "git -C prs-src log -1 --format=%s FETCH_HEAD | grep -q 'Merge pull request #1 from feature'"
check "shows as merged"             bash -c "curl -s $URL/alice/prs/pull/1 | grep -q 'state-badge merged' && curl -s '$URL/alice/prs/pulls?state=closed' | grep -q '#1 opened'"
check "open count back to 0"        bash -c "! curl -s $URL/alice/prs | grep -q '<span class=\"count\">'"
post $A /alice/prs/pull/1/delete-branch csrf=$(pr_csrf $A 1) >/dev/null
check "branch deleted"              bash -c "! git ls-remote $PRS 2>/dev/null | grep -q refs/heads/feature && curl -s $URL/alice/prs/pull/1 | grep -q 'deleted the feature branch'"
check "merged PR still has commits" bash -c "curl -s $URL/alice/prs/pull/1/commits | grep -q 'Improve g'"
check "conflicting PR opens"        test "$(open_pr $B conflict 'Shout')" = "303 $URL/alice/prs/pull/2"
check "conflicts reported"          bash -c "curl -s $URL/alice/prs/pull/2 | grep -q 'This branch has conflicts' && curl -s $URL/alice/prs/pull/2 | grep -q '<li class=\"mono\">f.txt</li>'"
BEFORE=$(main_sha)
post $A /alice/prs/pull/2/merge csrf=$(pr_csrf $A 2) method=merge >/dev/null
check "conflicting merge refused"   bash -c "grep -q \"can&#39;t be merged\\|Couldn&#39;t merge\" $W/post.html && [ \"\$(git ls-remote $PRS refs/heads/main 2>/dev/null | cut -f1)\" = '$BEFORE' ]"
post $B /alice/prs/pull/2/state csrf=$(pr_csrf $B 2) action=close "body=Not needed" >/dev/null
check "author closes"               bash -c "curl -s $URL/alice/prs/pull/2 | grep -q 'state-badge closed' && curl -s $URL/alice/prs/pull/2 | grep -q 'closed this'"
post $B /alice/prs/pull/2/state csrf=$(pr_csrf $B 2) action=reopen >/dev/null
check "author reopens"              bash -c "curl -s $URL/alice/prs/pull/2 | grep -q 'state-badge open' && curl -s $URL/alice/prs/pull/2 | grep -q 'reopened this'"
check "anonymous can't comment"     bash -c "[ \"\$(curl -s -o /dev/null -w '%{http_code}' --data 'csrf=x&body=hi' $URL/alice/prs/pull/2/comment)\" = 303 ] && ! curl -s $URL/alice/prs/pull/2 | grep -q '>hi<'"
open_pr $B squash-me 'Squash these' >/dev/null
post $A /alice/prs/pull/3/merge csrf=$(pr_csrf $A 3) method=squash >/dev/null
check "squash: one commit"          bash -c "git -C prs-src fetch -q $PRS main && [ \$(git -C prs-src log -1 --format=%P FETCH_HEAD | wc -w) = 1 ] && git -C prs-src log -1 --format=%s FETCH_HEAD | grep -qx 'Squash these (#3)'"
check "squash: PR author credited"  bash -c "[ \"\$(git -C prs-src log -1 --format='%an|%cn' FETCH_HEAD)\" = 'bob|alice' ]"
check "squash: content merged"      bash -c "[ \"\$(git -C prs-src show FETCH_HEAD:s.txt)\" = \$'s1\\ns2' ]"
open_pr $B rebase-me 'Rebase these' >/dev/null
(cd prs-src && git checkout -q rebase-me && echo r3 >>r.txt && git commit -qam "Rebase three" && git push -q $PRS rebase-me && git checkout -q main)
check "pushes update the PR"        bash -c "curl -s $URL/alice/prs/pull/4/commits | grep -q 'Rebase three'"
post $A /alice/prs/pull/4/merge csrf=$(pr_csrf $A 4) method=rebase >/dev/null
check "rebase: commits replayed"    bash -c "git -C prs-src fetch -q $PRS main && [ \"\$(git -C prs-src log -3 --format=%s FETCH_HEAD | tr '\\n' '|')\" = 'Rebase three|Rebase two|Rebase one|' ]"
check "rebase: linear history"      bash -c "[ \$(git -C prs-src log -1 --format=%P FETCH_HEAD | wc -w) = 1 ] && [ \"\$(git -C prs-src log -1 --format='%an|%cn' FETCH_HEAD)\" = 'test|alice' ]"
check "merged PR files still work"  bash -c "curl -s $URL/alice/prs/pull/4/files | grep -q 'r.txt'"
check "private repo PRs hidden"     test "$(curl -s -o /dev/null -w '%{http_code}' $URL/alice/secret/pulls)" = 404

echo "== Issues"
$BIN admin create-repo alice/tracker >/dev/null
TR=/alice/tracker
icsrf() { csrf $1 "$TR/issues/$2"; }
check "default labels created"      bash -c "curl -s $URL$TR/labels | grep -q 'good first issue' && curl -s $URL$TR/labels | grep -q '7 labels'"
check "new issue needs sign-in"     bash -c "curl -s -o /dev/null -w '%{redirect_url}' $URL$TR/issues/new | grep -q '/login?next=/alice/tracker/issues/new'"
R=$(post $B $TR/issues csrf=$(csrf $B $TR/issues/new) "title=Crash when the plate is empty" $'body=Steps:\n\n1. Load an empty plate\n\nThanks @alice')
check "bob opens issue #1"          test "$R" = "303 $URL$TR/issues/1"
check "empty title refused"         bash -c "post $B $TR/issues csrf=$(csrf $B $TR/issues/new) title=' ' >/dev/null; grep -q 'Give the issue a title' $W/post.html"
post $A $TR/issues csrf=$(csrf $A $TR/issues/new) "title=Document the CSV format" >/dev/null
check "issue page renders"          bash -c "curl -s $URL$TR/issues/1 | grep -q 'Load an empty plate'"
check "@mention links to user"      bash -c "curl -s $URL$TR/issues/1 | grep -q '<a href=\"/alice\" rel=\"noopener noreferrer\">@alice</a>'"
post $A $TR/issues/1/comment csrf=$(icsrf $A 1) "body=Same as #2? Not #99." >/dev/null
check "#ref links existing issue"   bash -c "curl -s $URL$TR/issues/1 | grep -q 'href=\"/alice/tracker/issues/2\"' && ! curl -s $URL$TR/issues/1 | grep -q 'issues/99'"
LABEL_BUG=$(db "select l.id from labels l join repos r on r.id = l.repo_id where r.name = 'tracker' and l.name = 'bug'")
LABEL_Q=$(db "select l.id from labels l join repos r on r.id = l.repo_id where r.name = 'tracker' and l.name = 'question'")
check "bob can't label"             test "$(post $B $TR/issues/1/labels csrf=$(icsrf $B 1) id=$LABEL_BUG | cut -d' ' -f1)" = 403
post $A $TR/issues/1/labels csrf=$(icsrf $A 1) id=$LABEL_BUG id=$LABEL_Q >/dev/null
check "labels applied"              bash -c "curl -s $URL$TR/issues/1 | grep -q 'added the bug label' && curl -s $URL$TR/issues | grep -q 'class=\"label-chip\"'"
post $A $TR/issues/1/labels csrf=$(icsrf $A 1) id=$LABEL_BUG >/dev/null
check "label removed"               bash -c "curl -s $URL$TR/issues/1 | grep -q 'removed the question label'"
BOB_ID=$(db "select id from users where username = 'bob'")
post $A $TR/issues/1/assignees csrf=$(icsrf $A 1) id=$BOB_ID >/dev/null
check "assigned"                    bash -c "curl -s $URL$TR/issues/1 | grep -q 'assigned bob' && curl -s $URL$TR/issues | grep -q 'assigned to bob'"
check "filter by label"             bash -c "curl -s '$URL$TR/issues?label=bug' | grep -q 'Crash when the plate' && ! curl -s '$URL$TR/issues?label=bug' | grep -q 'Document the CSV'"
check "search titles"               bash -c "curl -s '$URL$TR/issues?q=csv' | grep -q 'Document the CSV' && ! curl -s '$URL$TR/issues?q=csv' | grep -q 'Crash when'"
check "search escapes wildcards"    bash -c "! curl -s '$URL$TR/issues?q=%25' | grep -q 'Crash when'"
check "issues tab count"            bash -c "curl -s $URL$TR | grep -A0 'Issues' | grep -q '<span class=\"count\">2</span>'"
post $B $TR/issues/2/comment csrf=$(icsrf $B 2) action=close >/dev/null
check "non-author can't close"      bash -c "[ \"\$(db \"select state from issues where number = 2 and repo_id = (select id from repos where name = 'tracker')\")\" = open ]"
post $B $TR/issues/1/comment csrf=$(icsrf $B 1) "body=Fixed locally" action=close >/dev/null
check "author comments and closes"  bash -c "curl -s $URL$TR/issues/1 | grep -q 'Fixed locally' && curl -s $URL$TR/issues/1 | grep -q 'closed this' && curl -s '$URL$TR/issues?state=closed' | grep -q 'Crash when'"
post $B $TR/issues/1/comment csrf=$(icsrf $B 1) action=reopen >/dev/null
check "author reopens"              bash -c "curl -s $URL$TR/issues/1 | grep -q 'reopened this'"
TRK=http://alice:$WEB_TOKEN@127.0.0.1:$PORT/alice/tracker.git
git init -q -b main tr-src && (cd tr-src && echo a >a && git add a && git commit -qm init && git push -q $TRK main \
    && git checkout -qb fix-crash && echo b >b && git add b && git commit -qm "Handle empty plates" -m "Fixes #1" && git push -q $TRK fix-crash)
check "PR shares numbering (#3)"    test "$(post $B $TR/pulls csrf=$(csrf $B "$TR/compare?base=main&head=fix-crash") base=main head=fix-crash 'title=Handle empty plates' 'body=See #2')" = "303 $URL$TR/pull/3"
check "issue URL redirects to PR"   bash -c "[ \"\$(curl -s -o /dev/null -w '%{http_code} %{redirect_url}' $URL$TR/issues/3)\" = '308 $URL$TR/pull/3' ]"
check "PR URL redirects to issue"   bash -c "[ \"\$(curl -s -o /dev/null -w '%{http_code} %{redirect_url}' $URL$TR/pull/1)\" = '308 $URL$TR/issues/1' ]"
check "PR body links #2"            bash -c "curl -s $URL$TR/pull/3 | grep -q 'href=\"/alice/tracker/issues/2\"'"
post $A $TR/pull/3/merge csrf=$(csrf $A $TR/pull/3) method=squash >/dev/null
check "merge closes 'Fixes #1'"     bash -c "curl -s $URL$TR/issues/1 | grep -q 'closed this in' && curl -s $URL$TR/issues/1 | grep -q 'href=\"/alice/tracker/pull/3\"' && [ \"\$(db \"select state from issues where number = 1 and repo_id = (select id from repos where name = 'tracker')\")\" = closed ]"
check "#2 only mentioned, open"     test "$(db "select state from issues where number = 2 and repo_id = (select id from repos where name = 'tracker')")" = open
R=$(post $A $TR/labels csrf=$(csrf $A $TR/labels) name=urgent color=#ff0000 description=Now)
check "create label"                bash -c "[ '$R' = '303 $URL$TR/labels' ] && curl -s $URL$TR/labels | grep -q 'urgent'"
check "bad label color refused"     bash -c "post $A $TR/labels csrf=$(csrf $A $TR/labels) name=evil 'color=red;x' >/dev/null; grep -q 'a color like' $W/post.html"
check "duplicate label refused"     bash -c "post $A $TR/labels csrf=$(csrf $A $TR/labels) name=URGENT color=#00ff00 >/dev/null; grep -q 'already exists' $W/post.html"
URGENT=$(db "select l.id from labels l join repos r on r.id = l.repo_id where r.name = 'tracker' and l.name = 'urgent'")
check "bob can't delete label"      test "$(post $B $TR/labels/$URGENT/delete csrf=$(csrf $B $TR/labels) | cut -d' ' -f1)" = 403
post $A $TR/labels/$URGENT/delete csrf=$(csrf $A $TR/labels) >/dev/null
check "alice deletes label"         bash -c "! curl -s $URL$TR/labels | grep -q 'urgent'"
check "private repo issues hidden"  test "$(curl -s -o /dev/null -w '%{http_code}' $URL/alice/secret/issues)" = 404

echo "---- $pass passed, $fail failed"
if grep -qiE " (ERROR|WARN) " "$W/server.log"; then
    echo "server log warnings/errors:"; grep -iE " (ERROR|WARN) " "$W/server.log" | head -10
fi
[ $fail = 0 ]
