// Host test of the rule the fix of DxgkCreateAllocation rests on (card #757,
// finding F10 of docs/reviews/win3d-driver.md).
//
// The kernel driver allocates its objects with an operator new of its own,
// viogpu/common/baseobj.cpp, that returns NULL when the pool has no memory.
// A compiler tests the result of a new expression before it runs the
// constructor only when that operator new is noexcept. Without it the
// constructor runs on the NULL pointer, and a NULL check of the caller comes
// too late. patches/kmd/kmd-check-create-allocation-memory.patch adds the
// noexcept. scripts/test_kmd_create_allocation.py takes the declaration out
// of the patch (-DPOOL_NEW_DECLARATION=...), or the one the patch removes for
// the "before", and compiles this file with it.
//
// The program prints "constructed-at-null" and exits 1 when the constructor
// ran on NULL (it is noinline and tests its own this pointer first, where the
// driver's constructor writes a member), "null" and exits 0 when the new
// expression gave NULL and no constructor ran. A compiler may also use the
// rule to drop the test of `this`: the constructor then writes to address 0
// and the program dies of a signal, which is the same "before".
#include <cstddef>
#include <cstdio>
#include <cstdlib>

enum POOL_TYPE { NonPagedPoolNx = 512, PagedPool = 1 };
#define _When_(condition, annotation)
#define __drv_reportError(text)
#define __cdecl

// The declaration of baseobj.h, from the patch.
void *__cdecl
POOL_NEW_DECLARATION

static bool pool_is_empty;

void *operator new(size_t Size, POOL_TYPE) POOL_NEW_NOEXCEPT
{
    return pool_is_empty ? nullptr : std::malloc(Size);
}

struct Allocation
{
    volatile int m_valid;
    __attribute__((noinline)) explicit Allocation(int valid);
    bool IsValid() const { return m_valid != 0; }
};

static volatile void *volatile seen_this;

Allocation::Allocation(int valid)
{
    seen_this = this;
    if (seen_this == nullptr)
    {
        std::puts("constructed-at-null");
        std::fflush(stdout);
        std::exit(1);
    }
    m_valid = valid;
}

int main()
{
    Allocation *first = new (NonPagedPoolNx) Allocation(1);
    if (!first || !first->IsValid())
    {
        std::puts("the allocation with memory failed");
        return 2;
    }
    pool_is_empty = true;
    // viogpu_allocation.cpp, DxgkCreateAllocation, with the patch:
    //   VioGpuAllocation *allocation = new (NonPagedPoolNx) VioGpuAllocation(adapter, &localExchange);
    //   if (!allocation) return STATUS_INSUFFICIENT_RESOURCES;
    Allocation *allocation = new (NonPagedPoolNx) Allocation(1);
    if (!allocation)
    {
        std::puts("null");
        return 0;
    }
    std::puts("not null");
    return 3;
}
