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
        echo "PASS $name ($(grep -m1 -oE '(omaforge|fatal|GraphQL|[Ee]rror|.*Permission denied).*' "$W/last.log"))"
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

echo "== Collaborators and repository settings"
echo carolpassword | $BIN admin create-user carol --password-stdin >/dev/null
$BIN admin add-ssh-key carol laptop --key-file "$W/carol.pub" >/dev/null
C=$W/carol.jar; curl -s -c $C --data 'login=carol&password=carolpassword' -o /dev/null $URL/login
CAROL_HTTP=http://carol:carolpassword@127.0.0.1:$PORT
code() { curl -s -o /dev/null -w '%{http_code}' -b "$1" "$URL$2"; }
check "settings: anonymous to login" bash -c "curl -s -o /dev/null -w '%{redirect_url}' $URL/alice/secret/settings | grep -q '/login?next=/alice/secret/settings'"
check "settings: bob gets 404"      test "$(code $B /alice/prs/settings)" = 404
check "settings: owner sees page"   bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -b $A $URL/alice/prs/settings) = 200 ] && curl -s -b $A $URL/alice/prs | grep -q '/alice/prs/settings'"
check "no Settings tab for bob"     bash -c "! curl -s -b $B $URL/alice/prs | grep -q '/alice/prs/settings'"
check "carol can't see secret"      bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -b $C $URL/alice/secret) = 404 ]"
refuse "carol can't clone secret"   git clone -q $CAROL_HTTP/alice/secret.git carol-1
R=$(post $A /alice/secret/settings/collaborators csrf=$(csrf $A /alice/secret/settings) username=carol role=read)
check "add carol with read"         test "$R" = "303 $URL/alice/secret/settings#collaborators"
check "read: carol browses"         bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -b $C $URL/alice/secret) = 200 ] && curl -s -b $C $URL/ | grep -q 'alice/secret'"
check "read: clone over HTTP"       git clone -q $CAROL_HTTP/alice/secret.git carol-read
check "read: clone over SSH"        env GIT_SSH_COMMAND="$(ssh_as carol)" git clone -q $SSH_URL/alice/secret.git carol-read-ssh
(cd carol-read && echo carol >carol.txt && git add carol.txt && git commit -qm "Carol was here")
refuse "read: push refused (HTTP)"  git -C carol-read push -q $CAROL_HTTP/alice/secret.git HEAD:carol
refuse "read: push refused (SSH)"   env GIT_SSH_COMMAND="$(ssh_as carol)" git -C carol-read push -q $SSH_URL/alice/secret.git HEAD:carol
check "read: no Settings for carol" bash -c "! curl -s -b $C $URL/alice/secret | grep -q '/alice/secret/settings'"
post $A /alice/secret/settings/collaborators csrf=$(csrf $A /alice/secret/settings) username=carol role=write >/dev/null
check "write: push over HTTP"       git -C carol-read push -q $CAROL_HTTP/alice/secret.git HEAD:carol
check "write: push over SSH"        env GIT_SSH_COMMAND="$(ssh_as carol)" git -C carol-read push -q $SSH_URL/alice/secret.git HEAD:carol-ssh
check "write: still no Settings"    test "$(code $C /alice/secret/settings)" = 404
post $A /alice/prs/settings/collaborators csrf=$(csrf $A /alice/prs/settings) username=carol role=write >/dev/null
(cd prs-src && git checkout -qb carol-change main && git pull -q $PRS main && echo c >c.txt && git add c.txt && git commit -qm "Carol's change" && git push -q $PRS carol-change && git checkout -q main)
NUM=$(post $B /alice/prs/pulls csrf=$(csrf $B "/alice/prs/compare?base=main&head=carol-change") base=main head=carol-change "title=Carol change" | grep -o '[0-9]*$')
check "write: merge button shown"   bash -c "curl -s -b $C $URL/alice/prs/pull/$NUM | grep -q 'Merge pull request'"
check "write: carol merges"         test "$(post $C /alice/prs/pull/$NUM/merge csrf=$(csrf $C /alice/prs/pull/$NUM) method=merge)" = "303 $URL/alice/prs/pull/$NUM"
# Write access to alice/prs doesn't extend to alice/tracker.
post $C /alice/tracker/issues/2/labels csrf=$(csrf $C /alice/tracker/issues/2) id=$LABEL_BUG >/dev/null
check "no triage without access"    test "$(db "select count(*) from issue_labels il join issues i on i.id = il.issue_id join repos r on r.id = i.repo_id where r.name = 'tracker' and i.number = 2")" = 0
CAROL_ID=$(db "select id from users where username = 'carol'")
post $A /alice/secret/settings/collaborators/$CAROL_ID/delete csrf=$(csrf $A /alice/secret/settings) >/dev/null
check "removed: access gone"        bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -b $C $URL/alice/secret) = 404 ]"
refuse "removed: clone refused"     git clone -q $CAROL_HTTP/alice/secret.git carol-2
check "unknown user refused"        bash -c "post $A /alice/secret/settings/collaborators csrf=\$(csrf $A /alice/secret/settings) username=nobody role=write >/dev/null; grep -q 'no user named' $W/post.html"
check "owner can't be added"        bash -c "post $A /alice/secret/settings/collaborators csrf=\$(csrf $A /alice/secret/settings) username=alice role=read >/dev/null; grep -q 'already has full access' $W/post.html"
check "bob can't add himself"       test "$(post $B /alice/secret/settings/collaborators csrf=$(csrf $B /settings/keys) username=bob role=write | cut -d' ' -f1)" = 404
check "CLI adds collaborator"       bash -c "$BIN admin add-collaborator alice/secret carol --role read | grep -q 'carol now has read access' && [ \$(curl -s -o /dev/null -w '%{http_code}' -b $C $URL/alice/secret) = 200 ]"
post $A /alice/later/settings csrf=$(csrf $A /alice/later/settings) "description=Now private" visibility=private >/dev/null
check "make private"                bash -c "grep -q 'Settings saved' $W/post.html && [ \$(curl -s -o /dev/null -w '%{http_code}' $URL/alice/later) = 404 ]"
post $A /alice/later/settings csrf=$(csrf $A /alice/later/settings) "description=Public again" visibility=public >/dev/null
check "make public, description"    bash -c "curl -s $URL/alice/later | grep -q 'Public again'"
post $A /alice/made-on-web/issues csrf=$(csrf $A /alice/made-on-web/issues/new) "title=Survives a rename" >/dev/null
R=$(post $A /alice/made-on-web/settings/rename csrf=$(csrf $A /alice/made-on-web/settings) name=renamed)
check "rename"                      test "$R" = "303 $URL/alice/renamed/settings"
check "old name gone, new works"    bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -b $A $URL/alice/made-on-web) = 404 ] && curl -s -b $A $URL/alice/renamed/issues/1 | grep -q 'Survives a rename'"
check "clone renamed repo"          git clone -q http://alice:$WEB_TOKEN@127.0.0.1:$PORT/alice/renamed.git renamed-clone
check "rename onto existing refused" bash -c "post $A /alice/renamed/settings/rename csrf=\$(csrf $A /alice/renamed/settings) name=hello >/dev/null; grep -q 'already exists' $W/post.html"
post $A /alice/renamed/settings/delete csrf=$(csrf $A /alice/renamed/settings) confirm=renamed >/dev/null
check "delete needs full name"      bash -c "grep -q 'Nothing was deleted' $W/post.html && [ -d $OMAFORGE_DATA/repos/alice/renamed.git ]"
check "delete needs CSRF"           test "$(post $A /alice/renamed/settings/delete csrf=nope confirm=alice/renamed | cut -d' ' -f1)" = 403
R=$(post $A /alice/renamed/settings/delete csrf=$(csrf $A /alice/renamed/settings) confirm=alice/renamed)
check "delete"                      bash -c "[ '$R' = '303 $URL/alice' ] && [ ! -e $OMAFORGE_DATA/repos/alice/renamed.git ] && [ \"\$(db \"select count(*) from repos where name = 'renamed'\")\" = 0 ] && [ \"\$(db \"select count(*) from issues where repo_id not in (select id from repos)\")\" = 0 ]"

echo "== gh CLI (GitHub-compatible API)"
if ! command -v gh >/dev/null || ! command -v openssl >/dev/null; then
    echo "SKIP gh/openssl not installed"
else
    # gh only talks HTTPS to hosts other than github.com, so put TLS in front of the server.
    TLS_PORT=${E2E_TLS_PORT:-3923}
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout $W/tls-key.pem -out $W/tls-cert.pem \
        -days 2 -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost" 2>/dev/null
    python3 -I $ROOT/tests/tls_proxy.py $TLS_PORT $PORT $W/tls-cert.pem $W/tls-key.pem & PROXY=$!
    trap 'kill $SERVER $PROXY 2>/dev/null' EXIT
    sleep 0.5
    mkdir -p $W/gh
    export GH_CONFIG_DIR=$W/gh GH_HOST=localhost:$TLS_PORT GH_NO_UPDATE_NOTIFIER=1 GH_PROMPT_DISABLED=1 GH_PAGER=cat NO_COLOR=1
    export SSL_CERT_FILE=$W/tls-cert.pem GIT_SSL_CAINFO=$W/tls-cert.pem
    BOB_TOKEN=$($BIN admin create-token bob gh)
    ALICE_GH() { GH_ENTERPRISE_TOKEN=$WEB_TOKEN gh "$@"; }
    BOB_GH() { GH_ENTERPRISE_TOKEN=$BOB_TOKEN gh "$@"; }
    export -f ALICE_GH BOB_GH; export WEB_TOKEN BOB_TOKEN
    GHR="-R alice/prs"
    check "gh api user"                 bash -c "ALICE_GH api user | grep -q '\"login\":\"alice\"'"
    check "gh repo view"                bash -c "ALICE_GH repo view alice/prs | grep -q 'name:.*alice/prs'"
    check "gh issue create"             bash -c "BOB_GH issue create $GHR -t 'Typo in the docs' -b 'It says teh.' | grep -q '/alice/prs/issues/'"
    ISSUE=$(BOB_GH issue list $GHR | awk '/Typo in the docs/{print $1}')
    check "gh issue list"               test -n "$ISSUE"
    check "gh issue view"               bash -c "BOB_GH issue view $ISSUE $GHR | grep -q 'It says teh.'"
    check "gh issue comment"            bash -c "ALICE_GH issue comment $ISSUE $GHR -b 'Good catch' | grep -q '#comment-' && ALICE_GH issue view $ISSUE $GHR --comments | grep -q 'Good catch'"
    check "gh issue edit (labels)"      bash -c "ALICE_GH issue edit $ISSUE $GHR --add-label bug --title 'Typo: teh' >/dev/null && ALICE_GH issue view $ISSUE $GHR | grep -q 'labels:.*bug' && ALICE_GH issue view $ISSUE $GHR | grep -q 'Typo: teh'"
    refuse "bob can't label via gh"     BOB_GH issue edit $ISSUE $GHR --add-label question
    check "gh issue close/reopen"       bash -c "BOB_GH issue close $ISSUE $GHR && BOB_GH issue list $GHR --state closed | grep -q 'Typo: teh' && BOB_GH issue reopen $ISSUE $GHR"
    (cd prs-src && git checkout -q main && git pull -q $PRS main && git checkout -qb fix-typo && echo "the" >docs.txt && git add docs.txt \
        && git commit -qm "Fix the typo" -m "Fixes #$ISSUE" && git push -q $PRS fix-typo && git checkout -qb two-cents main && echo x >x.txt && git add x.txt && git commit -qm x && git push -q $PRS two-cents && git checkout -q main)
    check "gh pr create"                bash -c "BOB_GH pr create $GHR -B main -H fix-typo -t 'Fix the typo' -b 'Small fix.' | grep -q '/alice/prs/pull/'"
    PR=$(BOB_GH pr list $GHR | awk '/Fix the typo/{print $1}')
    check "gh pr list"                  test -n "$PR"
    check "gh pr view"                  bash -c "BOB_GH pr view $PR $GHR | grep -q 'Small fix.' && BOB_GH pr view $PR $GHR | grep -q 'state:.*OPEN'"
    check "gh pr diff"                  bash -c "BOB_GH pr diff $PR $GHR | grep -q '^+the$'"
    check "gh pr checks (none)"         bash -c "BOB_GH pr checks $PR $GHR 2>&1 | grep -q 'no checks reported'"
    check "gh pr status"                bash -c "BOB_GH pr status $GHR | grep -q 'Relevant pull requests'"
    check "gh pr edit (author)"         bash -c "BOB_GH pr edit $PR $GHR --title 'Fix the docs typo' >/dev/null && BOB_GH pr view $PR $GHR | grep -q 'Fix the docs typo'"
    refuse "gh: no self-approval"       BOB_GH pr review $PR $GHR --approve
    check "gh pr review --approve"      bash -c "ALICE_GH pr review $PR $GHR --approve >/dev/null && ALICE_GH pr view $PR $GHR | grep -q 'alice (Approved)'"
    check "gh pr comment"               bash -c "BOB_GH pr comment $PR $GHR -b 'Thanks!' | grep -q '#comment-'"
    refuse "bob can't merge via gh"     BOB_GH pr merge $PR $GHR --squash
    check "gh pr merge --squash"        bash -c "ALICE_GH pr merge $PR $GHR --squash && ALICE_GH pr view $PR $GHR | grep -q 'state:.*MERGED'"
    check "merge closed 'Fixes #N'"     bash -c "ALICE_GH issue list $GHR --state closed | grep -q 'Typo: teh'"
    BOB_GH pr create $GHR -B main -H two-cents -t 'Two cents' -b '' >/dev/null 2>&1
    PR2=$(BOB_GH pr list $GHR | awk '/Two cents/{print $1}')
    check "gh pr close/reopen"          bash -c "BOB_GH pr close $PR2 $GHR && BOB_GH pr list $GHR --state closed | grep -q 'Two cents' && BOB_GH pr reopen $PR2 $GHR && BOB_GH pr list $GHR | grep -q 'Two cents'"
    check "gh repo clone"               bash -c "cd $W && ALICE_GH repo clone alice/prs gh-clone -- -q && test -f gh-clone/docs.txt"
    # gh can't match a git remote whose host has a port (the test port here) to GH_HOST,
    # so `gh pr checkout` only works against a real deployment on port 443.
    echo "SKIP gh pr checkout (gh doesn't support hosts with a port for this; fine on 443)"
    refuse "private repo hidden in gh"  BOB_GH repo view alice/secret
    check "bad token rejected"          bash -c "GH_ENTERPRISE_TOKEN=omf_nope gh api user 2>&1 | grep -q 'Bad credentials'"
fi

echo "== Branch protection"
$BIN admin create-repo alice/guarded >/dev/null
GUARD=http://alice:$WEB_TOKEN@127.0.0.1:$PORT/alice/guarded.git
git init -q -b main guard-src && (cd guard-src && echo one >f && git add f && git commit -qm one && git push -q $GUARD main)
GS=$(csrf $A /alice/guarded/settings)
post $A /alice/guarded/settings/branches csrf=$GS pattern=main require_pull_request=on required_approvals=1 block_force_push=on block_deletion=on >/dev/null
post $A /alice/guarded/settings/branches csrf=$GS 'pattern=release/*' required_approvals=0 block_force_push=on block_deletion=on >/dev/null
check "rules listed in settings"    bash -c "curl -s -b $A $URL/alice/guarded/settings | grep -q 'changes through pull requests · 1 approving review · no force pushes' && curl -s -b $A $URL/alice/guarded/settings | grep -q 'release/\*'"
check "bad pattern refused"         bash -c "post $A /alice/guarded/settings/branches csrf=$GS 'pattern=../x' >/dev/null; grep -q 'pattern like release' $W/post.html"
(cd guard-src && echo two >>f && git commit -qam two)
BLOCKED_SHA=$(git -C guard-src rev-parse HEAD)
check "direct push refused"         bash -c "! git -C guard-src push $GUARD main 2>$W/push.err && grep -q 'main is protected: changes must go through a pull request' $W/push.err"
check "refused push over SSH too"   bash -c "! GIT_SSH_COMMAND='$(ssh_as alice)' git -C guard-src push $SSH_URL/alice/guarded.git main 2>$W/push.err && grep -q 'changes must go through a pull request' $W/push.err"
check "refused objects not stored"  bash -c "! git --git-dir $OMAFORGE_DATA/repos/alice/guarded.git cat-file -e $BLOCKED_SHA 2>/dev/null"
check "unprotected branch pushes"   git -C guard-src push -q $GUARD HEAD:feature
check "create protected release/1"  git -C guard-src push -q $GUARD HEAD~1:refs/heads/release/1
check "fast-forward release/1"      git -C guard-src push -q $GUARD HEAD:release/1
check "force push refused"          bash -c "! git -C guard-src push -f $GUARD HEAD~1:release/1 2>$W/push.err && grep -q 'release/1 is protected: force pushes' $W/push.err"
check "deleting refused"            bash -c "! git -C guard-src push $GUARD :release/1 2>$W/push.err && grep -q \"release/1 is protected and can't be deleted\" $W/push.err"
# `release/*` doesn't reach into deeper paths, so this branch is unprotected.
check "pattern doesn't nest"        git -C guard-src push -q $GUARD HEAD:release/next/hotfix
check "unprotected delete works"    git -C guard-src push -q $GUARD :release/next/hotfix
PRN=$(post $A /alice/guarded/pulls csrf=$(csrf $A "/alice/guarded/compare?base=main&head=feature") base=main head=feature title=Two | grep -o '[0-9]*$')
check "merge blocked w/o approval"  bash -c "curl -s -b $A $URL/alice/guarded/pull/$PRN | grep -q 'Merging is blocked' && ! curl -s -b $A $URL/alice/guarded/pull/$PRN | grep -q 'Merge pull request</button>'"
post $A /alice/guarded/pull/$PRN/merge csrf=$(csrf $A /alice/guarded/pull/$PRN) method=merge >/dev/null
check "forced merge refused"        bash -c "grep -q 'needs 1 approving review' $W/post.html && [ \$(git ls-remote $GUARD refs/heads/main 2>/dev/null | cut -f1) != $BLOCKED_SHA ]"
QUERY="{\"query\":\"{repository(owner:\\\"alice\\\",name:\\\"guarded\\\"){pullRequest(number:$PRN){mergeStateStatus baseRef{branchProtectionRule{requiredApprovingReviewCount}}}}}\"}"
curl -s -H "Authorization: token $WEB_TOKEN" -H 'content-type: application/json' --data "$QUERY" $URL/api/graphql >$W/gql.json
check "API says BLOCKED"            bash -c "grep -q '\"mergeStateStatus\":\"BLOCKED\"' $W/gql.json && grep -q '\"requiredApprovingReviewCount\":1' $W/gql.json"
post $B /alice/guarded/pull/$PRN/review csrf=$(csrf $B /alice/guarded/pull/$PRN) state=approved >/dev/null
check "reader's approval not enough" bash -c "curl -s -b $A $URL/alice/guarded/pull/$PRN | grep -q 'Merging is blocked'"
$BIN admin add-collaborator alice/guarded carol --role write >/dev/null
post $C /alice/guarded/pull/$PRN/review csrf=$(csrf $C /alice/guarded/pull/$PRN) state=approved >/dev/null
check "writer's approval unblocks"  bash -c "curl -s -b $A $URL/alice/guarded/pull/$PRN | grep -q 'Merge pull request</button>'"
check "merge into protected main"   test "$(post $A /alice/guarded/pull/$PRN/merge csrf=$(csrf $A /alice/guarded/pull/$PRN) method=merge)" = "303 $URL/alice/guarded/pull/$PRN"
check "bob can't change rules"      test "$(post $B /alice/guarded/settings/branches csrf=$(csrf $B /settings/keys) pattern=x | cut -d' ' -f1)" = 404

echo "== Webhooks"
HOOK_PORT=${E2E_HOOK_PORT:-3924}
HOOK_LOG=$W/webhooks.jsonl; : >$HOOK_LOG
python3 -I $ROOT/tests/webhook_receiver.py $HOOK_PORT $HOOK_LOG s3cret & RECEIVER=$!
trap 'kill $SERVER $PROXY $RECEIVER 2>/dev/null' EXIT
sleep 0.3
# wait_for <jq-ish python expr>: until a logged delivery matches (5 s).
wait_for() { for _ in $(seq 50); do python3 -I -c "
import json,sys
rows=[json.loads(l) for l in open('$HOOK_LOG')]
sys.exit(0 if any($1 for r in rows) else 1)" && return 0; sleep 0.1; done; return 1; }
export HOOK_LOG; export -f wait_for
GS=$(csrf $A /alice/guarded/settings)
R=$(post $A /alice/guarded/settings/webhooks csrf=$GS url=http://127.0.0.1:$HOOK_PORT/hook secret=s3cret event=push event=pull_request event=issues event=issue_comment)
check "add webhook"                 test "$R" = "303 $URL/alice/guarded/settings#webhooks"
check "ping delivered, signed"      wait_for "r['event']=='ping' and r['signature_ok'] and r['user_agent'].startswith('omaforge-Hookshot')"
post $A /alice/guarded/settings/webhooks csrf=$GS url=http://127.0.0.1:$HOOK_PORT/fail secret=s3cret event=issues >/dev/null
(cd guard-src && git fetch -q $GUARD main && git checkout -q -B release/1 FETCH_HEAD 2>/dev/null; git checkout -q -b release/2 && echo r >r && git add r && git commit -qm "Release two" && git push -q $GUARD release/2)
check "push event"                  wait_for "r['event']=='push' and r['payload']['ref']=='refs/heads/release/2' and r['payload']['created'] and r['payload']['pusher']['name']=='alice' and r['payload']['head_commit']['message']=='Release two'"
(cd guard-src && echo r2 >>r && git commit -qam "Second commit" && git push -q $GUARD release/2)
check "push lists new commits"      wait_for "r['event']=='push' and r['payload']['ref']=='refs/heads/release/2' and not r['payload']['created'] and [c['message'] for c in r['payload']['commits']]==['Second commit'] and r['payload']['repository']['full_name']=='alice/guarded'"
PR2N=$(post $A /alice/guarded/pulls csrf=$(csrf $A "/alice/guarded/compare?base=main&head=release/2") base=main head=release/2 "title=Release two" | grep -o '[0-9]*$')
check "pull_request opened"         wait_for "r['event']=='pull_request' and r['payload']['action']=='opened' and r['payload']['number']==$PR2N"
post $C /alice/guarded/pull/$PR2N/review csrf=$(csrf $C /alice/guarded/pull/$PR2N) state=approved >/dev/null
post $A /alice/guarded/pull/$PR2N/merge csrf=$(csrf $A /alice/guarded/pull/$PR2N) method=merge >/dev/null
check "merge recorded as a push"    wait_for "r['event']=='push' and r['payload']['ref']=='refs/heads/main' and 'Merge pull request #$PR2N' in r['payload']['head_commit']['message']"
check "pull_request closed/merged"  wait_for "r['event']=='pull_request' and r['payload']['action']=='closed' and r['payload']['pull_request']['merged']"
post $B /alice/guarded/issues csrf=$(csrf $B /alice/guarded/issues/new) "title=Webhook test" >/dev/null
ISS=$(db "select max(number) from issues where repo_id = (select id from repos where name = 'guarded')")
check "issues opened"               wait_for "r['event']=='issues' and r['payload']['action']=='opened' and r['payload']['issue']['title']=='Webhook test' and r['payload']['sender']['login']=='bob' and r['path']=='/hook'"
post $B /alice/guarded/issues/$ISS/comment csrf=$(csrf $B /alice/guarded/issues/$ISS) "body=Hello hooks" >/dev/null
check "issue_comment created"       wait_for "r['event']=='issue_comment' and r['payload']['comment']['body']=='Hello hooks'"
check "failing hook retried"        bash -c "sleep 1; [ \"\$(db \"select status || ':' || attempts || ':' || response_status from deliveries d join webhooks w on w.id = d.webhook_id where w.url like '%/fail' and d.event = 'issues' order by d.id limit 1\")\" = 'pending:1:500' ]"
check "event filter respected"      bash -c "[ \"\$(db \"select count(*) from deliveries d join webhooks w on w.id = d.webhook_id where w.url like '%/fail' and d.event = 'push'\")\" = 0 ]"
check "deliveries shown"            bash -c "curl -s -b $A $URL/alice/guarded/settings | grep -q 'HTTP 200 in' && curl -s -b $A $URL/alice/guarded/settings | grep -q 'HTTP 500 in'"
DID=$(db "select min(d.id) from deliveries d join webhooks w on w.id = d.webhook_id where w.url like '%/hook' and d.event = 'ping'")
post $A /alice/guarded/settings/deliveries/$DID/redeliver csrf=$(csrf $A /alice/guarded/settings) >/dev/null
check "redeliver"                   bash -c "for _ in \$(seq 50); do [ \$(grep -c '\"event\": \"ping\"' $HOOK_LOG) -ge 2 ] && exit 0; sleep 0.1; done; exit 1"
check "bob can't add webhooks"      test "$(post $B /alice/guarded/settings/webhooks csrf=$(csrf $B /settings/keys) url=http://x.example event=push | cut -d' ' -f1)" = 404

echo "---- $pass passed, $fail failed"
if grep -qiE " (ERROR|WARN) " "$W/server.log"; then
    echo "server log warnings/errors:"; grep -iE " (ERROR|WARN) " "$W/server.log" | head -10
fi
[ $fail = 0 ]
