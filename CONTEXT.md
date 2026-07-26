# Worktrees

Worktrees manages navigation and lifecycle operations for the worktrees and branches of the repository containing the current directory.

## Language

**Primary worktree**:
Git’s first non-bare worktree for a repository, used as the stable repository home.
_Avoid_: Main worktree

**Target branch**:
The configured branch into which topic work is integrated and against which safe removal is judged.
_Avoid_: Main branch, default branch

**Topic worktree**:
A non-primary worktree attached to a named branch that may be integrated into the target branch and removed.
_Avoid_: Secondary worktree, non-main worktree

**Removal**:
Unregistering and deleting a topic worktree while retaining its branch.
_Avoid_: Branch deletion, cleanup
